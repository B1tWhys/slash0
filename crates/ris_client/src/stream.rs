use crate::messages::{
    ClientMessage, RisMessage, RisMessageBody, RisSubscribe, ServerMessage, SubscriptionFilters,
};
use anyhow::Context;
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use metrics::Unit::Seconds;
use metrics::{Counter, Histogram, histogram};
use std::time::{Duration, SystemTime};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

/// Encapsulating the metrics handles in this struct lets us amortize the cost of registering
/// the metrics
struct MessageMetrics {
    /// 1 json blob received from RIS counts as one message
    messages_total: Counter,
    /// Each RIS message can contain multiple updates, each containing multiple prefixes
    prefixes_announced_total: Counter,
    /// Each RIS message can contain multiple withdrawals
    prefixes_withdrawn_total: Counter,
    /// Histogram representing the age of the message(s) when they arrive at the slash0 server. The
    /// age is based on the timestamp recorded by ris-live when the BGP update reaches the route
    /// collector
    message_age_on_receipt: Histogram,
}

impl MessageMetrics {
    fn new() -> Self {
        Self {
            messages_total: metrics::counter!("slash0_ris_messages_total"),
            prefixes_announced_total: metrics::counter!("slash0_ris_prefixes_announced_total"),
            prefixes_withdrawn_total: metrics::counter!("slash0_ris_prefixes_withdrawn_total"),
            message_age_on_receipt: histogram!(
                description: "Age of messages when they arrive at slash0. Measured from when the \
                update arrived at the ris-live collector",
                unit: Seconds,
                "slash0_ris_message_age_on_receipt"),
        }
    }

    /// Counts every relayed message, plus the announced and withdrawn prefixes
    /// carried by UPDATE
    async fn record(&mut self, message: &RisMessage) {
        let now_ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let msg_age = now_ts - message.timestamp;
        self.message_age_on_receipt.record(msg_age);

        self.messages_total.increment(1);

        if let RisMessageBody::Update(update) = &message.body {
            let announced: usize = update
                .announcements
                .iter()
                .flatten()
                .map(|announcement| announcement.prefixes.len())
                .sum();
            let withdrawn = update.withdrawals.iter().flatten().count();

            self.prefixes_announced_total.increment(announced as u64);
            self.prefixes_withdrawn_total.increment(withdrawn as u64);
        }
    }
}

/// RIS Live websocket endpoint
const RIS_LIVE_STREAM_URL: &str = "wss://ris-live.ripe.net/v1/ws/";

/// Ring-buffer capacity the broadcast channel retains for lagging receivers.
/// A receiver that falls this far behind sees `RecvError::Lagged` and resumes at
/// the oldest retained message. Sized for the per-update fan-out RIS Live bursts.
const CHANNEL_CAPACITY: usize = 10_000;

/// If RIS terminates our connection, wait this long before attempting to reconnect
/// (for politeness). Maybe exponential backoff would be better, but this is fine for
/// our usecase
const RIS_RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// As requested, put my project name in a "client" query param, in case RIS live wants to get in
/// touch
const RIS_LIVE_CLIENT_NAME: &str = "github.com/B1tWhys/slash0";

/// Opens a RIS Live stream filtered by `filters` and broadcasts every decoded
/// `ris_message` to all subscribers of the returned sender. Attach a consumer
/// with [`broadcast::Sender::subscribe`]; each consumer independently sees the
/// full stream.
///
/// The connection is established before returning, so transport and HTTP-status
/// failures surface here rather than on the background task. That task then runs
/// until RIS Live closes the stream, regardless of how many receivers are
/// attached.
pub async fn subscribe(
    filters: SubscriptionFilters,
    shutdown_signal: CancellationToken,
) -> anyhow::Result<broadcast::Receiver<RisMessage>> {
    let mut records = connect_to_ris(&filters).await?;
    let mut metrics = MessageMetrics::new();

    let (tx, rx) = broadcast::channel(CHANNEL_CAPACITY);
    let ingest_tx = tx.clone();
    tokio::spawn(async move {
        let mut done = false;
        // Keep reconnecting to RIS until the shutdown signal is fired
        while !done {
            // Keep parsing records received from RIS until the connection gets closed
            loop {
                // Wait for either the next record from RIS, or the shutdown signal
                let record = tokio::select! {
                    // Handle graceful shutdown
                    _ = shutdown_signal.cancelled() => {
                        done = true;
                        break;
                    }
                    // Receive a record from RIS live. If it's empty then the stream is closed and we try reconnecting
                    record = records.next() => {
                        match record {
                            Some(r) => r,
                            None => break
                        }
                    }
                };

                // Extract the RIS message if it's present, or bail from the loop and reconnect to RIS if we got
                // an error (or the connection has already closed on us)
                let record = match record {
                    Ok(Message::Text(record)) => record,
                    Ok(Message::Close(close_frame)) => {
                        warn!(?close_frame, "RIS-live closed the websocket connection");
                        break;
                    }
                    Err(err) => {
                        warn!(%err, "RIS Live stream read error, closing");
                        break;
                    }
                    _ => {
                        info!(
                            ?record,
                            "Received unexpected record type from RIS-live, ignoring it"
                        );
                        continue;
                    }
                };

                // Parse the message JSON, and publish it through the stream!
                match serde_json::from_str::<ServerMessage>(record.as_ref()) {
                    Ok(ServerMessage::RisMessage(message)) => {
                        metrics.record(&message).await;
                        // A send error only means no receivers are currently attached; consumers may
                        // subscribe later, so keep ingesting.
                        let _ = ingest_tx.send(message);
                    }
                    Ok(ServerMessage::RisError(err)) => {
                        warn!(message = %err.message, "RIS Live reported an error");
                    }
                    Ok(other) => debug!(?other, "ignoring non-message RIS Live envelope"),
                    Err(err) => warn!(%err, ?record, "skipping unparseable RIS Live record"),
                }
            }

            info!(
                "RIS Live stream ended, pausing for {:.1}s before attempting to reconnect...",
                RIS_RECONNECT_DELAY.as_secs_f32()
            );
            tokio::select! {
                // Wait a little bit before reconnecting. No need to hammer RIS super hard if they're
                // not happy with us
                _ = tokio::time::sleep(RIS_RECONNECT_DELAY) => {
                    match connect_to_ris(&filters).await {
                        Ok(new_stream) => {
                            info!("Successfully reconnected to RIS, resuming streaming events");
                            records = new_stream;
                        }
                        Err(err) => {
                           warn!(%err, "Failed to reconnect to RIS, will try again soon");
                        }
                    }
                }
                _ = shutdown_signal.cancelled() => {
                    info!("Received shutdown signal while waiting between RIS reconnection attempts. Abandoning the reconnect loop and exiting!");
                    break
                }
            }
        }
    });

    Ok(rx)
}

async fn connect_to_ris(
    filters: &SubscriptionFilters,
) -> anyhow::Result<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>> {
    let subscribe_header = serde_json::to_string(filters)?;

    let mut url = Url::parse(RIS_LIVE_STREAM_URL).expect("The RIS live URL is valid");
    url.query_pairs_mut()
        .append_pair("client", RIS_LIVE_CLIENT_NAME);

    info!(%subscribe_header, %url, "subscribing to RIS Live");

    let (ws_stream, _) = connect_async(url.as_str()).await?;
    info!("RIS websocket handshake successful");

    let (mut write, records) = ws_stream.split();
    write
        .send(Message::Text(
            serde_json::to_string(&ClientMessage::RisSubscribe(RisSubscribe {
                filters: filters.clone(),
                socket_options: None,
            }))
            .context("Failed to serialize RIS subscribe message")?
            .into(),
        ))
        .await
        .context("Failed to send RIS subscribe message")?;

    Ok(records)
}

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use metrics::Unit::Seconds;
use metrics::{Counter, Histogram, histogram};
use std::time::SystemTime;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::messages::{
    ClientMessage, RisMessage, RisMessageBody, RisSubscribe, ServerMessage, SubscriptionFilters,
};

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
) -> anyhow::Result<broadcast::Receiver<RisMessage>> {
    let subscribe_header = serde_json::to_string(&filters)?;
    debug!(%subscribe_header, "subscribing to RIS Live");

    let (ws_stream, _) = connect_async(RIS_LIVE_STREAM_URL).await?;
    info!("RIS websocket handshake successful");

    let (mut write, mut records) = ws_stream.split();
    write
        .send(Message::Text(
            serde_json::to_string(&ClientMessage::RisSubscribe(RisSubscribe {
                filters,
                socket_options: None,
            }))
            .context("Failed to serialize RIS subscribe message")?
            .into(),
        ))
        .await
        .context("Failed to send RIS subscribe message")?;

    let mut metrics = MessageMetrics::new();

    let (tx, rx) = broadcast::channel(CHANNEL_CAPACITY);
    let ingest_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(record) = records.next().await {
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
                    debug!(
                        ?record,
                        "Received unexpected record type from RIS-live, ignoring it"
                    );
                    continue;
                }
            };

            match serde_json::from_str::<ServerMessage>(record.as_ref()) {
                Ok(ServerMessage::RisMessage(message)) => {
                    metrics.record(&message).await;
                    // A send error only means no receivers are currently
                    // attached; consumers may subscribe later, so keep ingesting.
                    let _ = ingest_tx.send(message);
                }
                Ok(ServerMessage::RisError(err)) => {
                    warn!(message = %err.message, "RIS Live reported an error");
                }
                Ok(other) => debug!(?other, "ignoring non-message RIS Live envelope"),
                Err(err) => warn!(%err, ?record, "skipping unparseable RIS Live record"),
            }
        }
        info!("RIS Live stream ended");
    });

    Ok(rx)
}

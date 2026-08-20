use futures::{StreamExt, TryStreamExt};
use metrics::Unit::Seconds;
use metrics::{Counter, Histogram, histogram};
use std::time::SystemTime;
use tokio::sync::broadcast;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;
use tracing::{debug, info, warn};

use crate::messages::{RisMessage, RisMessageBody, ServerMessage, SubscriptionFilters};

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
    fn record(&self, message: &RisMessage) {
        let now_ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f32();
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

/// RIS Live newline-delimited JSON streaming endpoint.
const RIS_LIVE_STREAM_URL: &str = "https://ris-live.ripe.net/v1/stream/?format=json";

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

    let response = reqwest::Client::new()
        .get(RIS_LIVE_STREAM_URL)
        .header("X-RIS-Subscribe", subscribe_header)
        .send()
        .await?
        .error_for_status()?;

    // RIS Live frames one `ServerMessage` per line; adapt the chunked byte
    // stream into those records so partial-chunk reassembly isn't our problem.
    let byte_stream = response.bytes_stream().map_err(std::io::Error::other);
    let mut records = FramedRead::new(StreamReader::new(byte_stream), LinesCodec::new());
    let metrics = MessageMetrics::new();

    let (tx, rx) = broadcast::channel(CHANNEL_CAPACITY);
    let ingest_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(record) = records.next().await {
            let record = match record {
                Ok(record) => record,
                Err(err) => {
                    warn!(%err, "RIS Live stream read error, closing");
                    break;
                }
            };

            match serde_json::from_str::<ServerMessage>(&record) {
                Ok(ServerMessage::RisMessage(message)) => {
                    metrics.record(&message);
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

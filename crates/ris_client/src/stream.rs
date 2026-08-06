use futures::{StreamExt, TryStreamExt};
use tokio::sync::broadcast;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;
use tracing::{debug, info, warn};

use crate::messages::{RisMessage, RisMessageBody, ServerMessage, SubscriptionFilters};

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
) -> anyhow::Result<broadcast::Sender<RisMessage>> {
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

    let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
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
                    record_metrics(&message);
                    // A send error only means no receivers are currently
                    // attached; consumers may subscribe later, so keep ingesting.
                    let _ = ingest_tx.send(message);
                }
                Ok(ServerMessage::RisError(err)) => {
                    warn!(message = %err.message, "RIS Live reported an error");
                }
                Ok(other) => debug!(?other, "ignoring non-message RIS Live envelope"),
                Err(err) => warn!(%err, "skipping unparseable RIS Live record"),
            }
        }
        info!("RIS Live stream ended");
    });

    Ok(tx)
}

/// Counts every relayed message, plus the announced and withdrawn prefixes
/// carried by UPDATEs. Counting prefixes rather than messages matches the trie
/// granularity: each prefix is one update to apply. Rates are derived downstream
/// (e.g. `rate()` in Prometheus). Zero-valued increments keep every series
/// present from the first message.
fn record_metrics(message: &RisMessage) {
    metrics::counter!("slash0_ris_messages_total").increment(1);

    if let RisMessageBody::Update(update) = &message.body {
        let announced: usize = update
            .announcements
            .iter()
            .flatten()
            .map(|announcement| announcement.prefixes.len())
            .sum();
        let withdrawn = update.withdrawals.iter().flatten().count();

        metrics::counter!("slash0_ris_announcements_total").increment(announced as u64);
        metrics::counter!("slash0_ris_withdrawals_total").increment(withdrawn as u64);
    }
}

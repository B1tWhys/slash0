use crate::messages::{RisMessage, ServerMessage};
use futures::StreamExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::BufReader;
use tokio::sync::broadcast;
use tokio_util::codec::{FramedRead, LinesCodec};
use tracing::error;

/// Create a stream of RIS events from a jsonl file. If `simulate_time` is set, the stream is
/// dolled out at a pace based on the timestamps of the events
pub async fn subscribe_from_file(
    file_path: &PathBuf,
    simulate_time: bool,
) -> anyhow::Result<broadcast::Receiver<RisMessage>> {
    let file = File::open(file_path).await?;
    let buf = BufReader::new(file);

    let lines = FramedRead::new(buf, LinesCodec::new());
    let messages = lines
        .filter_map(|r| {
            futures::future::ready(r.ok().and_then(|line| {
                serde_json::from_str::<ServerMessage>(&line)
                    .inspect_err(|e| error!("Failed to parse the event line: {line:?} {e:?}"))
                    .ok()
            }))
        })
        .filter_map(|m| {
            futures::future::ready(match m {
                ServerMessage::RisMessage(body) => Some(body),
                _ => None,
            })
        });

    let (tx, rx) = broadcast::channel::<RisMessage>(1000);
    let ingest_tx = tx.clone();
    tokio::spawn(async move {
        let mut last_timestamp_sec: Option<f64> = None;
        tokio::pin!(messages);
        while let Some(message) = messages.next().await {
            if let Some(last_ts) = last_timestamp_sec {
                let delay = Duration::from_secs_f64((message.timestamp - last_ts).max(0.0));
                if simulate_time {
                    tokio::time::sleep(delay).await;
                }
            }
            last_timestamp_sec = Some(message.timestamp);
            ingest_tx.send(message).unwrap();
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_invalid_path() {
        let path: PathBuf = "/foo/bar/baz".into();
        let result = subscribe_from_file(&path, false).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("No such file or directory"));
    }

    #[tokio::test]
    async fn test_reading_mock_events() {
        tracing_subscriber::fmt()
            .with_env_filter("ris_client=debug")
            .with_test_writer()
            .init();

        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("src/sample_messages.json");

        let mut rx = subscribe_from_file(&path, false).await.unwrap();
        let mut messages = vec![];
        while let Ok(msg) = rx.recv().await {
            messages.push(msg);
        }

        assert_eq!(messages.len(), 206)
    }
}

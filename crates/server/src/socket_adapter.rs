use anyhow::Context;
use axum::extract::ws::{Message, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use slash0_core::wire::Slash0Message;
use tracing::info;

type WsTx = SplitSink<WebSocket, Message>;
type WsRx = SplitStream<WebSocket>;

pub trait SocketAdapter {
    async fn next(&mut self) -> anyhow::Result<Option<Slash0Message>>;
    async fn send(&mut self, message: &Slash0Message) -> anyhow::Result<()>;
}

pub struct WebSocketAdapter {
    rx: WsRx,
    tx: WsTx,
}

impl WebSocketAdapter {
    pub fn new(socket: WebSocket) -> Self {
        let (tx, rx) = socket.split();
        Self { rx, tx }
    }
}

impl SocketAdapter for WebSocketAdapter {
    async fn next(&mut self) -> anyhow::Result<Option<Slash0Message>> {
        if let Some(result) = self.rx.next().await {
            let raw_msg = result.context("Failed to read message from socket")?;
            let data = raw_msg.into_data();

            let message: Slash0Message = postcard::from_bytes(&data).with_context(|| {
                format!(
                    "Failed to deserialize message: {:?}",
                    data.slice(..data.len().min(500))
                )
            })?;
            info!(?message, "Received Slash0WebsocketMessage");
            return Ok(Some(message));
        }
        Ok(None)
    }

    async fn send(&mut self, message: &Slash0Message) -> anyhow::Result<()> {
        let body = postcard::to_allocvec(&message)?;
        let message = Message::Binary(body.into());
        self.tx.send(message).await?;

        Ok(())
    }
}

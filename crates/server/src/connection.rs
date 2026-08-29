use crate::route_table::RouteTable;
use crate::socket_adapter::{SocketAdapter, WebSocketAdapter};
use ris_client::messages::{RisMessage, RisMessageBody};
use slash0_core::prefix::IpVersion;
use slash0_core::timestamp::Timestamp;
use slash0_core::tree::RadixTree;
use slash0_core::wire::{Slash0Message, ThinBgpUpdate};
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::{info, warn};

pub struct ConnectionCtx<Socket: SocketAdapter = WebSocketAdapter> {
    pub route_table: Arc<RouteTable>,
    pub socket: Socket,
    #[allow(dead_code)]
    pub client_addr: SocketAddr,
}

impl<S: SocketAdapter> ConnectionCtx<S> {
    fn new(socket: S, route_table: Arc<RouteTable>, client_addr: SocketAddr) -> Self {
        ConnectionCtx {
            route_table,
            socket,
            client_addr,
        }
    }
}

/// Connection is a state machine representing the full lifecycle on the server-side of the
/// websocket opened from the client, starting from right after the websocket has been initialized
/// by Axum (i.e. the upgrade from http -> websocket is completed).
///
/// The overall flow is:
/// 1. A connection is created in the Idle state
/// 2. The client sends a [Slash0Message::SubscribeRequest] with the IP type they're interested in (v4/v6)
/// 3. The state machine moves to Syncronizing while the client is brought up to speed. First we send down
///    a snapshot of the current state of the relevant route table (converted from the thick table stored
///    in the server to a thin representation suitable for use in the browser/shader on the client side)
/// 4. Once the initial snapshot is sent, we transition to the Active state
/// 5. A continuous stream of thin events are sent down to the client, to keep the client in sync with the
///    server. This stream of events starts from exactly the point in time snapshot that was sent down
///    to the client.
///
///    Note: At time of writing, the route table is fully locked while a copy of the trie is created in
///    memory, and the messages flowing to all clients are interrupted till that process is completed.
///    On my laptop, this is currently taking ~60ms which is completely unacceptable. In the future,
///    a much more sophisticated solution will need to be implemented to allow clients to connect
///    without stopping the world.
/// 6. A client can stop the messages flowing by sending Unsubscribe, returning to the Idle state // TODO
/// 7. A client can also just directly send Subscribe from the Active state to return to Synchronizing.
///    This allows switching between ip v4/v6 without having to go through the Idle state.
///    // TODO: Should subscribing to the already subscriped ipversion be a no-op? Or trigger a
///    // re-sync?
///
///
/// ## State diagram
///
/// ```
///      Start    +--------------------Unsubscribe (TODO)-------------------+
///        |      |                                                         |
///        v      v                                                         |
/// +----------------+                +------------------+             +----+--------------+
/// |                |                |                  |             |                   |
/// |      Idle      +---Subscribe--->|  Synchronizing   +------------>|      Active       |
/// |                |                |                  |             |                   |
/// +--------+-------+                +--------+---------+             +----+----+---------+
///          |                                 |      ^                     |    |
///          |                                 v      +------Subscribe------+    |
///          |                        +----------------+                         |
///          |                        |                |                         |
///          +----------------------->|    Closing     |<------------------------+
///                                   |                |
///                                   +--------+-------+
///                                            |
///                                            v
///                                           End
/// ```
pub enum Connection {
    Idle(IdleState),
    Synchronizing(SynchronizingState),
    Active(ActiveState),
    Closing,
}

impl Debug for Connection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let state_name = self.state_name();

        let mut dbg = f.debug_struct(&format!("Connection::{state_name}"));
        match self {
            Connection::Idle(_) => {}
            Connection::Synchronizing(state) => {
                dbg.field("ip_version", &state.ip_version);
            }
            Connection::Active(_) => {}
            Connection::Closing => {}
        }

        Ok(())
    }
}

impl Connection {
    pub fn new() -> Self {
        info!("New connection constructed");
        Connection::Idle(IdleState {})
    }

    pub fn state_name(&self) -> &'static str {
        match self {
            Connection::Idle(_) => "Idle",
            Connection::Synchronizing(_) => "Synchronizing",
            Connection::Active(_) => "Active",
            Connection::Closing => "Closing",
        }
    }

    pub async fn run(
        mut self,
        socket: impl SocketAdapter,
        route_table: Arc<RouteTable>,
        client_addr: SocketAddr,
    ) {
        let mut ctx = ConnectionCtx::new(socket, route_table, client_addr);

        loop {
            self = match self {
                Connection::Idle(s) => s.advance(&mut ctx).await,
                Connection::Synchronizing(s) => s.advance(&mut ctx).await,
                Connection::Active(s) => s.advance(&mut ctx).await,
                Connection::Closing => break,
            };

            info!("Connection state transitioned to: {}", self.state_name())
        }
    }
}

pub struct IdleState {}

impl IdleState {
    pub async fn advance<S: SocketAdapter>(self, ctx: &mut ConnectionCtx<S>) -> Connection {
        loop {
            let message = match ctx.socket.next().await {
                Ok(Some(message)) => message,
                Ok(None) => {
                    info!("Received Ok(None) message while Idle. Closing the connection");
                    return Connection::Closing;
                }
                Err(err) => {
                    warn!(?err, "Received fucking incomprehensible message while idle");
                    continue;
                }
            };
            info!(?message, "Message received while idle");

            if let Slash0Message::SubscribeRequest { ip_version } = message {
                return Connection::Synchronizing(SynchronizingState { ip_version });
            }
        }
    }
}

pub struct SynchronizingState {
    ip_version: IpVersion,
}

impl SynchronizingState {
    async fn advance<S: SocketAdapter>(self, ctx: &mut ConnectionCtx<S>) -> Connection {
        let (thick_tree, message_stream) = ctx.route_table.subscribe(self.ip_version);
        let thin_tree = RadixTree::from_thick_tree(&thick_tree);
        let snapshot = Slash0Message::TrieSnapshot {
            ip_version: self.ip_version,
            tree: thin_tree,
        };
        ctx.socket
            .send(&snapshot)
            .await
            .expect("TODO: Handle this properly");

        Connection::Active(ActiveState {
            ip_version: self.ip_version,
            message_stream,
        })
    }
}

pub struct ActiveState {
    ip_version: IpVersion,
    message_stream: tokio::sync::broadcast::Receiver<RisMessage>,
}

impl ActiveState {
    async fn advance<S: SocketAdapter>(self, ctx: &mut ConnectionCtx<S>) -> Connection {
        let stream = BroadcastStream::new(self.message_stream);
        let chunked_stream = stream.chunks_timeout(512, Duration::from_millis(16));
        tokio::pin!(chunked_stream);

        loop {
            tokio::select! {
                Some(ris_chunk) = chunked_stream.next() => {
                    let chunk = ris_chunk.iter().flatten();
                    let msg = ris_chunk_to_slash0_framed_msg(self.ip_version, chunk);
                    if let Err(e) = ctx.socket.send(&msg).await {
                        warn!(%e, "Failed to send message, closing connection");
                        return Connection::Closing;
                    }
                }
                received = ctx.socket.next() => {
                    match received {
                        Ok(Some(Slash0Message::SubscribeRequest { ip_version })) => {
                            if self.ip_version != ip_version {
                                return Connection::Synchronizing(SynchronizingState { ip_version })
                            }
                        },
                        Ok(Some(_)) => {},
                        Ok(None) => {
                            info!("Connection closed");
                            return Connection::Closing;
                        },
                        Err(e) => {
                            info!(%e, "Error received, closing connection");
                            return Connection::Closing;
                        }
                    }
                }
            }
        }
    }
}

fn ris_chunk_to_slash0_framed_msg<'a>(
    ip_version: IpVersion,
    ris_chunk: impl Iterator<Item = &'a RisMessage>,
) -> Slash0Message {
    let body = ris_chunk
        .flat_map(|m| ris_message_to_slash0_messages(ip_version, m))
        .flatten()
        .collect::<Vec<_>>();
    Slash0Message::ThinBgpUpdateFrame(body)
}

fn ris_message_to_slash0_messages(
    ip_version: IpVersion,
    update: &RisMessage,
) -> Option<impl IntoIterator<Item = ThinBgpUpdate>> {
    let timestamp = Timestamp::from_sec(update.timestamp);

    match update.body {
        RisMessageBody::Update(ref bgp_update) => {
            Some(bgp_update.to_thin_updates(timestamp, ip_version))
        }
        _ => None,
    }
}

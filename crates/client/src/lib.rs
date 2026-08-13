use crate::render::{RenderState, start};
use anyhow::{Context, anyhow, bail};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use log::warn;
use slash0_core::node::Node;
use slash0_core::prefix::IpVersion;
use slash0_core::slab::VecSlab;
use slash0_core::thin::ThinData;
use slash0_core::tree::RadixTree;
use slash0_core::wire::{Slash0Message, UpdateType};
use thiserror::Error;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;

use crate::stats_for_nerds::StatsState;
use wasm_bindgen::prelude::*;

mod render;
mod stats_for_nerds;
pub mod time;

const CANVAS_ELEMENT_ID: &str = "slash0";

pub type Tree = RadixTree<ThinData, VecSlab<Node<ThinData>>>;
pub type Tx = SplitSink<WebSocket, Message>;
pub type Rx = SplitStream<WebSocket>;

fn set_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&info.to_string().into());
    }));
}

#[wasm_bindgen(start)]
pub fn main() {
    set_panic_hook();
    console_log::init_with_level(log::Level::Info).ok();
    spawn_local(async {
        run().await;
    });
}

async fn run() {
    let mut state = ClientState::Initializing(InitializingState {});

    let mut stats = StatsState::default();
    loop {
        if stats.is_time_for_report() {
            stats.report(&state);
        }
        state = match state {
            ClientState::Initializing(s) => s.run().await,
            ClientState::Connecting(s) => s.run().await,
            ClientState::Connected(s) => s.run().await,
            ClientState::Synchronizing(s) => s.run().await,
            ClientState::Subscribed(s) => s.run(&mut stats).await,
            ClientState::Error(_) => break,
        }
        .unwrap_or_else(ClientState::Error)
    }
}

pub enum ClientState {
    /// The canvas/GPU hasn't been setup yet
    Initializing(InitializingState),
    /// The websocket connection is not open yet
    Connecting(ConnectingState),
    /// The websocket is connected, but we don't have a tree pulled down yet and we're not
    /// receiving events (and if we do, for some reason, they're ignored)
    Connected(ConnectedState),
    /// We've subscribed to an ip type, but the trie hasn't been pulled down yet
    Synchronizing(SynchronizingState),
    /// We've got a trie, and are continuously updating it from websocket events
    Subscribed(SubscribedState),
    /// We failed somehow
    Error(Slash0Error),
}

impl ClientState {
    pub fn is_err(&self) -> bool {
        matches!(self, ClientState::Initializing(_))
    }
}

#[derive(Debug, Error)]
pub enum Slash0Error {
    #[error("Failed to initialize the canvas: {0:?}")]
    CanvasInitializationError(JsValue),
    #[error("Failed to open the websocket connection to slash0: {0:?}")]
    WebsocketError(#[from] anyhow::Error),
    #[error("Failed to download route table state: {msg}")]
    SynchronizationError { msg: String, source: anyhow::Error },
}

#[derive(Debug)]
pub struct InitializingState {}

impl InitializingState {
    pub async fn run(self) -> Result<ClientState, Slash0Error> {
        let render_state = start(CANVAS_ELEMENT_ID)
            .await
            .map_err(Slash0Error::CanvasInitializationError)?;

        Ok(ClientState::Connecting(ConnectingState { render_state }))
    }
}

pub struct ConnectingState {
    pub render_state: RenderState,
}

impl ConnectingState {
    pub async fn run(self) -> Result<ClientState, Slash0Error> {
        let ws = WebSocket::open("/ws").map_err(|e| Slash0Error::WebsocketError(e.into()))?;
        let (tx, rx) = ws.split();

        Ok(ClientState::Connected(ConnectedState {
            render_state: self.render_state,
            ip_version: IpVersion::V4,
            tx,
            rx,
        }))
    }
}

pub struct ConnectedState {
    pub render_state: RenderState,
    pub ip_version: IpVersion,
    pub tx: Tx,
    pub rx: Rx,
}

impl ConnectedState {
    pub async fn run(mut self) -> Result<ClientState, Slash0Error> {
        send(
            &mut self.tx,
            Slash0Message::SubscribeRequest {
                ip_version: self.ip_version,
            },
        )
        .await
        .map_err(|e| Slash0Error::SynchronizationError {
            msg: "Failed to send subscribe request".to_string(),
            source: anyhow!(e),
        })?;

        Ok(ClientState::Synchronizing(SynchronizingState {
            render_state: self.render_state,
            ip_version: self.ip_version,
            tx: self.tx,
            rx: self.rx,
        }))
    }
}

pub struct SynchronizingState {
    pub render_state: RenderState,
    pub ip_version: IpVersion,
    pub tx: Tx,
    pub rx: Rx,
}

impl SynchronizingState {
    pub async fn run(mut self) -> Result<ClientState, Slash0Error> {
        loop {
            match receive(&mut self.rx).await {
                Ok(Some(m)) => match m {
                    Slash0Message::TrieSnapshot { ip_version, tree } => {
                        if ip_version != self.ip_version {
                            warn!(
                                "Received an {} trie when I expected an {} trie. Ignoring it",
                                ip_version, self.ip_version
                            );
                            continue;
                        }

                        return Ok(ClientState::Subscribed(SubscribedState {
                            render_state: self.render_state,
                            ip_version,
                            tx: self.tx,
                            rx: self.rx,
                            tree,
                            updates_since_sweep: 0,
                        }));
                    }
                    Slash0Message::SubscribeRequest { .. } => {}
                    Slash0Message::ThinBgpUpdate(_) => {}
                },
                Ok(None) => {
                    warn!("Connection closed while attempting to sync. Reconnecting...");
                    return Ok(ClientState::Connecting(ConnectingState {
                        render_state: self.render_state,
                    }));
                }
                Err(e) => return Err(Slash0Error::WebsocketError(e)),
            }
        }
    }
}

pub struct SubscribedState {
    pub render_state: RenderState,
    pub ip_version: IpVersion,
    pub tx: Tx,
    pub rx: Rx,
    pub tree: Tree,
    updates_since_sweep: usize,
}

impl SubscribedState {
    pub async fn run(mut self, stats: &mut StatsState) -> Result<ClientState, Slash0Error> {
        let msg = match receive(&mut self.rx).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                warn!(
                    "Connection closed from the server side while attempting to receive the next tree update. Reconnecting..."
                );
                return Ok(ClientState::Connecting(ConnectingState {
                    render_state: self.render_state,
                }));
            }
            Err(e) => {
                warn!("Failed to receive update, reconnecting: {e:?}");
                return Ok(ClientState::Connecting(ConnectingState {
                    render_state: self.render_state,
                }));
            }
        };

        let Slash0Message::ThinBgpUpdate(update) = msg else {
            // Just some noise. Ignore it
            return Ok(ClientState::Subscribed(self));
        };

        let thin_data = ThinData {
            timestamp: update.timestamp,
        };

        let t = &mut self.tree;
        stats.record_update();
        match update.update_type {
            UpdateType::ANNOUNCE => {
                t.insert(update.prefix, update.timestamp, thin_data, &mut |_| {});
            }
            UpdateType::WITHDRAW => {
                t.withdraw(update.prefix, update.timestamp, &mut |_| {});
            }
        };

        self.updates_since_sweep += 1;
        if self.updates_since_sweep > 5000 {
            let pre_sweep_node_count = t.node_count();
            t.sweep_tombstones(&mut |_| {});
            let post_sweep_node_count = t.node_count();
            let swept_nodes = pre_sweep_node_count - post_sweep_node_count;
            stats.record_swept_nodes(swept_nodes);
            self.updates_since_sweep = 0;
        }

        Ok(ClientState::Subscribed(self))
    }
}

async fn send(tx: &mut SplitSink<WebSocket, Message>, body: Slash0Message) -> anyhow::Result<()> {
    tx.send(Message::Bytes(postcard::to_allocvec(&body).with_context(
        || format!("Failed to serialize message: {:?}", &body),
    )?))
    .await?;
    Ok(())
}

async fn receive(rx: &mut SplitStream<WebSocket>) -> anyhow::Result<Option<Slash0Message>> {
    let Some(raw_result) = rx.next().await else {
        return Ok(None);
    };

    match raw_result? {
        Message::Text(txt) => {
            bail!(
                "Unexpectedly received a text message from the server: {}",
                txt
            );
        }
        Message::Bytes(bytes) => Ok(Some(postcard::from_bytes::<Slash0Message>(&bytes)?)),
    }
}

use crate::render::RenderState;
use crate::time::now_timestamp;
use anyhow::{Context, anyhow, bail};
use futures::channel::oneshot;
use futures::stream::{SplitSink, SplitStream};
use futures::{FutureExt, SinkExt, StreamExt, select};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use log::warn;
use slash0_core::node::Node;
use slash0_core::prefix::IpVersion;
use slash0_core::slab::VecSlab;
use slash0_core::thin::ThinData;
use slash0_core::tree::RadixTree;
use slash0_core::wire::{Slash0Message, UpdateType};
use std::pin::pin;
use thiserror::Error;
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

mod render;
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

    loop {
        state = match state {
            ClientState::Initializing(s) => s.run().await,
            ClientState::Connecting(s) => s.run().await,
            ClientState::Connected(s) => s.run().await,
            ClientState::Synchronizing(s) => s.run().await,
            ClientState::Subscribed(s) => s.run().await,
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
        let render_state = RenderState::init(CANVAS_ELEMENT_ID, "fs_main")
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

                        // Seed the GPU slab buffer from the freshly downloaded
                        // tree. The per-frame render loop in SubscribedState
                        // takes over drawing from here.
                        self.render_state.upload_slab(&tree.slab);

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
    /// Runs the steady-state loop: apply websocket updates to the tree as they
    /// arrive, and draw one frame per browser animation frame. The two are
    /// decoupled - draws happen on the display's cadence regardless of update
    /// rate, and each frame samples the live clock so the shader's time-based
    /// fade animates even when no updates are arriving.
    pub async fn run(self) -> Result<ClientState, Slash0Error> {
        let SubscribedState {
            mut render_state,
            mut rx,
            mut tree,
            mut updates_since_sweep,
            tx: _tx,
            ip_version: _,
        } = self;

        // Hold the animation-frame future across iterations and re-arm it only
        // after it fires. AnimationFrameGuard already makes recreating it each
        // iteration safe -- dropping a still-pending frame cancels its callback --
        // but that would churn a boxed allocation plus a request/cancel
        // animation-frame pair on every websocket message. Persisting it
        // registers one callback and lets it run.
        let mut frame = next_animation_frame().boxed_local().fuse();

        loop {
            let mut recv = pin!(receive(&mut rx).fuse());

            select! {
                message = recv => match message {
                    Ok(Some(Slash0Message::ThinBgpUpdate(update))) => {
                        let mut dirty = Vec::new();
                        let thin_data = ThinData {
                            timestamp: update.timestamp,
                        };
                        match update.update_type {
                            UpdateType::ANNOUNCE => {
                                tree.insert(update.prefix, update.timestamp, thin_data, &mut |idx| {
                                    dirty.push(idx)
                                });
                            }
                            UpdateType::WITHDRAW => {
                                tree.withdraw(update.prefix, update.timestamp, &mut |idx| {
                                    dirty.push(idx)
                                });
                            }
                        }

                        updates_since_sweep += 1;
                        if updates_since_sweep > 5000 {
                            tree.sweep_tombstones(&mut |idx| dirty.push(idx));
                            updates_since_sweep = 0;
                        }

                        render_state.update(&dirty);
                    }
                    // Anything else on the wire is noise at this point.
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        warn!("Connection closed by the server. Reconnecting...");
                        return Ok(ClientState::Connecting(ConnectingState { render_state }));
                    }
                    Err(e) => {
                        warn!("Failed to receive update, reconnecting: {e:?}");
                        return Ok(ClientState::Connecting(ConnectingState { render_state }));
                    }
                },
                _ = frame => {
                    if let Err(e) = render_state.render(tree.root(), now_timestamp(), &tree.slab) {
                        warn!("Frame draw failed: {e:?}");
                    }
                    frame = next_animation_frame().boxed_local().fuse();
                }
            }
        }
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

/// Cancels a pending `requestAnimationFrame` when dropped, so a callback that
/// fires after its future is dropped never runs into a freed closure.
struct AnimationFrameGuard {
    window: web_sys::Window,
    handle: i32,
}

impl Drop for AnimationFrameGuard {
    fn drop(&mut self) {
        let _ = self.window.cancel_animation_frame(self.handle);
    }
}

/// Resolves on the next browser animation frame.
async fn next_animation_frame() {
    let (sender, receiver) = oneshot::channel::<()>();
    let closure = ScopedClosure::once(move |_timestamp: f64| {
        let _ = sender.send(());
    });
    let window = web_sys::window().expect("no window");
    let handle = window
        .request_animation_frame(closure.as_ref().unchecked_ref())
        .expect("request_animation_frame failed");
    // Declared after `closure` so it drops first: on early drop the guard
    // cancels the callback before `closure` is freed. `closure` itself stays
    // alive until the callback fires (unblocking the await below).
    let _guard = AnimationFrameGuard { window, handle };
    let _ = receiver.await;
}

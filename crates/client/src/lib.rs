use anyhow::{Context, bail};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use log::{debug, info, warn};
use slash0_core::node::Node;
use slash0_core::prefix::IpVersion;
use slash0_core::slab::{Slab, VecSlab};
use slash0_core::thin::ThinData;
use slash0_core::tree::RadixTree;
use slash0_core::wire::{Slash0Message, UpdateType};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::spawn_local;
use web_sys::js_sys::{Array, Promise};
use web_sys::window;

mod render;

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
        if let Err(err) = render::_start("slash0").await {
            log::error!("render init failed: {err:?}");
        }
    });
    garbo_main();
}

// TODO: Everything below this is just garbage experimentation while I figure out how wasm works. I'll need
// to be rewritten properly, i'm sure

const TREE_STATS_ELEMENT_ID: &str = "tree-stats";
fn display_metrics(tree: &Mutex<Tree>, metrics: &Arc<MetricsState>) -> anyhow::Result<()> {
    let tree = tree.lock().unwrap();
    let document = web_sys::window()
        .context("Couldn't get window")?
        .document()
        .context("Couldn't get document")?;
    let tree_stats = document.get_element_by_id(TREE_STATS_ELEMENT_ID).unwrap();

    let elements = [
        format!(
            "Update count: {}",
            metrics.total_updates.load(Ordering::SeqCst)
        ),
        format!("Node count: {}", tree.slab.size()),
        format!("Slab size (Bytes): {}", tree.slab.size_capacity()),
        format!("Sweep count: {}", tree.sweep_count()),
        format!(
            "Reclaimed nodes count: {}",
            metrics.reclaimed_nodes.load(Ordering::SeqCst)
        ),
    ]
    .map(|s| {
        let elem = document.create_element("li").unwrap();
        elem.set_text_content(Some(&s));
        elem
    });

    tree_stats.replace_children_with_node(&(Array::from_iter(elements)));

    Ok(())
}

// TODO: This will be configurable later, obviously;
const IP_VERSION: IpVersion = IpVersion::V4;

type Tree = RadixTree<ThinData, VecSlab<Node<ThinData>>>;

#[derive(Debug, Default)]
struct MetricsState {
    pub total_updates: AtomicUsize,
    pub reclaimed_nodes: AtomicUsize,
}

pub fn garbo_main() {
    let ws = WebSocket::open("/ws").unwrap();

    spawn_local(async move {
        let (mut write, mut read) = ws.split();

        let tree = match download_tree(&mut write, &mut read, IP_VERSION).await {
            Ok(tree) => Arc::new(Mutex::new(tree)),
            Err(e) => {
                warn!(
                    "Failed to load tree state. TODO: Do something sensible here: {}",
                    e
                );
                return;
            }
        };

        let tree_copy = Arc::clone(&tree);
        let metrics_state = Arc::new(MetricsState::default());
        let metrics_clone = Arc::clone(&metrics_state);
        spawn_local(async move {
            let tree = tree_copy;
            loop {
                display_metrics(&tree, &metrics_clone).expect("Somehow failed to update UI");
                sleep(Duration::from_secs(1)).await;
            }
        });

        info!("Radix trie initialized! Initiating continuous replication...");

        let mut updates_since_sweep: usize = 0;
        loop {
            let Ok(Some(Slash0Message::ThinBgpUpdate(update))) = receive(&mut read).await else {
                continue;
            };

            let thin_data = ThinData {
                timestamp: update.timestamp,
            };
            let mut t = tree.lock().unwrap();
            match update.update_type {
                UpdateType::ANNOUNCE => {
                    t.insert(update.prefix, update.timestamp, thin_data, &mut |_| {});
                }
                UpdateType::WITHDRAW => {
                    t.withdraw(update.prefix, update.timestamp, &mut |_| {});
                }
            }

            metrics_state.total_updates.fetch_add(1, Ordering::Relaxed);
            updates_since_sweep += 1;
            if updates_since_sweep > 100000 {
                let mut delta = t.node_count() as usize;
                t.sweep_tombstones(&mut |_| {});
                delta -= t.node_count() as usize;
                metrics_state
                    .reclaimed_nodes
                    .fetch_add(delta, Ordering::SeqCst);
                updates_since_sweep = 0;
            }
            drop(t);
        }
    })
}

async fn download_tree(
    tx: &mut SplitSink<WebSocket, Message>,
    rx: &mut SplitStream<WebSocket>,
    ip_version: IpVersion,
) -> anyhow::Result<Tree> {
    send(tx, Slash0Message::SubscribeRequest { ip_version }).await?;

    loop {
        match receive(rx).await {
            Ok(Some(Slash0Message::TrieSnapshot {
                ip_version: ipv,
                tree,
            })) => {
                if ip_version != ipv {
                    info!(
                        "Got tree snapshot with ip_version: {:?} when expecting one of type: {:?}. Ignoring.",
                        ipv, ip_version
                    );
                    continue;
                }
                return Ok(tree);
            }
            Ok(Some(msg)) => {
                debug!(
                    "Got unexpected message while waiting for tree snapshot. Ignoring: {:?}",
                    msg
                );
            }
            Ok(None) => {
                debug!(
                    "Connection was closed when attempting to synchronize with the server. Abandoning attempt"
                );
                bail!("Connectin closed!");
            }
            Err(err) => return Err(err.context("Failed to receive the trie message")),
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

async fn sleep(duration: Duration) {
    let _ = Promise::new(&mut |resolve, _| {
        window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                &resolve,
                duration.as_millis().min(i32::MAX as u128) as i32,
            )
            .unwrap();
    })
    .await;
}

use crate::time::now_ms;
use crate::{ClientState, SubscribedState};
use anyhow::Context;
use slash0_core::slab::Slab;
use std::time::{Duration, SystemTime};
use wasm_bindgen_futures::js_sys::Array;

const TREE_STATS_ELEMENT_ID: &str = "tree-stats";

pub struct StatsState {
    last_reported_ts_ms: SystemTime,
    updates_received: usize,
    last_reported_updates_received: usize,
    reclaimed_nodes: usize,
}

impl Default for StatsState {
    fn default() -> Self {
        Self {
            last_reported_ts_ms: now_ms(),
            updates_received: Default::default(),
            last_reported_updates_received: Default::default(),
            reclaimed_nodes: Default::default(),
        }
    }
}

impl StatsState {
    pub fn report(&mut self, client_state: &ClientState) {
        let now = now_ms();
        let sec_since_last_update = now
            .duration_since(self.last_reported_ts_ms)
            .expect("Should be able to compare the timestamps");
        let updates_since_last_report = self.updates_received - self.last_reported_updates_received;

        let updates_per_sec =
            updates_since_last_report as f32 / sec_since_last_update.as_secs_f32();

        let mut stats = vec![
            format!("Update count: {}", self.updates_received),
            format!("Update rate: {updates_per_sec:0.3}/s"),
        ];
        if let ClientState::Subscribed(state) = client_state {
            self.push_tree_stats(&mut stats, state)
        };

        Self::render_stats_to_dom(stats).unwrap();
        self.last_reported_ts_ms = now;
        self.last_reported_updates_received = self.updates_received;
    }

    pub fn record_update(&mut self) {
        self.updates_received += 1;
    }

    pub fn record_swept_nodes(&mut self, swept_nodes: u32) {
        self.reclaimed_nodes += swept_nodes as usize;
    }

    pub fn is_time_for_report(&self) -> bool {
        now_ms()
            .duration_since(self.last_reported_ts_ms)
            .expect("The times should be comparable")
            >= Duration::from_secs(1)
    }

    fn push_tree_stats(&self, out_vec: &mut Vec<String>, client_state: &SubscribedState) {
        let tree = &client_state.tree;
        out_vec.extend([
            format!("Updates since sweep: {}", client_state.updates_since_sweep),
            format!("Total reclaimed nodes: {}", self.reclaimed_nodes),
            format!("Node count: {}", tree.slab.size()),
            format!("Slab size (Bytes): {}", tree.slab.size_capacity()),
            format!("Sweep count: {}", tree.sweep_count()),
        ])
    }

    fn render_stats_to_dom(lines: Vec<String>) -> anyhow::Result<()> {
        let document = web_sys::window()
            .context("Couldn't get window")?
            .document()
            .context("Couldn't get document")?;
        let tree_stats = document.get_element_by_id(TREE_STATS_ELEMENT_ID).unwrap();
        let elements = lines.into_iter().map(|s| {
            let elem = document.create_element("li").unwrap();
            elem.set_text_content(Some(&s));
            elem
        });

        tree_stats.replace_children_with_node(&(Array::from_iter(elements)));

        Ok(())
    }
}

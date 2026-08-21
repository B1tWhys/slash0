use bgpkit_parser::models::ElemType;
use bgpkit_parser::{BgpkitParser, Filter};
use ipnet::IpNet;
use slash0_core::node::Node;
use slash0_core::prefix::{IpVersion, Prefix};
use slash0_core::slab::{Slab, VecSlab};
use slash0_core::thick::ThickData;
use slash0_core::timestamp::Timestamp;
use slash0_core::tree::RadixTree;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::{Level, field, info, info_span, instrument, span, warn};

use ris_client::messages::{RisMessage, RisMessageBody};

/// Server-side radix trie carrying full BGP metadata per node.
type ThickTree = RadixTree<ThickData, VecSlab<Node<ThickData>>>;

/// Capacity of the re-broadcast channel a subscriber falls back on. A consumer
/// that lags this far behind sees `RecvError::Lagged` and needs a fresh
/// subscription; sized to absorb the per-update RIS Live bursts.
const UPDATE_CHANNEL_CAPACITY: usize = 10_000;

/// Owns the IPv4 and IPv6 route tries and applies RIS Live BGP updates to them.
///
/// The tries are private and never handed out directly. [`subscribe`] is the
/// only way to read state out: it returns an owned snapshot of a trie plus a
/// receiver for the updates applied after that snapshot, aligned so the stream
/// resumes exactly where the snapshot ends. Each family has its own lock so
/// reading one doesn't block ingest of the other.
///
/// The snapshot is returned as an `Arc` rather than by value so this signature
/// survives the eventual move to non-blocking (arc-swap) snapshotting, where the
/// snapshot is an atomically-loaded `Arc` instead of a lock-and-clone.
///
/// [`subscribe`]: RouteTable::subscribe
pub struct RouteTable {
    v4: Mutex<ThickTree>,
    v6: Mutex<ThickTree>,
    /// Every ingested message is re-broadcast here after it has been applied, so
    /// a subscriber's stream begins right after its snapshot. Re-emitting after
    /// applying is what keeps a new receiver from starting ahead of the snapshot.
    updates: broadcast::Sender<RisMessage>,
}

impl RouteTable {
    fn new() -> Self {
        Self {
            v4: Mutex::new(RadixTree::new(VecSlab::new())),
            v6: Mutex::new(RadixTree::new(VecSlab::new())),
            updates: broadcast::channel(UPDATE_CHANNEL_CAPACITY).0,
        }
    }

    /// Builds a route table and spawns a background task that ingests `receiver`
    /// until the RIS stream closes. The returned handle can be cloned freely and
    /// used to [`subscribe`](Self::subscribe); the ingest task holds its own
    /// `Arc`, so ingest continues regardless of how many handles remain.
    pub fn spawn(receiver: broadcast::Receiver<RisMessage>) -> Arc<Self> {
        let table = Arc::new(Self::new());
        let ingest = Arc::clone(&table);
        tokio::spawn(async move { ingest.run(receiver).await });
        table
    }

    #[instrument(skip_all)]
    pub fn add_routes_from(&self, bgpkit_parser: BgpkitParser<impl Read>) {
        let mut v4_routes = HashMap::new();
        let mut v6_routes = HashMap::new();
        info!("Begin collecting routes from bgpkit_parser");
        for route in bgpkit_parser.add_filters(&[Filter::Type(ElemType::ANNOUNCE)]) {
            let ts = Timestamp::from_sec(route.timestamp);
            match route.prefix.prefix {
                IpNet::V4(net) => {
                    let prefix =
                        Prefix::from_address(net.addr().to_bits().into(), net.prefix_len() as u32);
                    let cur_ts = v4_routes.entry(prefix).or_insert(ts);
                    *cur_ts = (*cur_ts).max(ts);
                }
                IpNet::V6(net) => {
                    let prefix =
                        Prefix::from_address(net.addr().to_bits().into(), net.prefix_len() as u32);
                    let cur_ts = v6_routes.entry(prefix).or_insert(ts);
                    *cur_ts = (*cur_ts).max(ts);
                }
            }
        }

        let v4_route_count = v4_routes.len();
        let v6_route_count = v4_routes.len();
        let total_mrt_route_count = v4_routes.len();

        info_span!(
            "Routes read from MRT, adding them to tries",
            v4_route_count,
            v6_route_count,
            total_mrt_route_count,
        )
        .in_scope(|| {
            self.bulk_insert_routes(IpVersion::V4, v4_routes);
            self.bulk_insert_routes(IpVersion::V6, v6_routes);
            self.sweep();
        });
    }

    fn bulk_insert_routes(&self, ip_version: IpVersion, routes: HashMap<Prefix, Timestamp>) {
        let mut trie = self.tree_for(ip_version).lock().expect("Lock corrupted");
        for (prefix, ts) in routes {
            // This pattern won't scale when thick data is, well... thicker. Can revisit later though.
            trie.insert(prefix, ts, ThickData { timestamp: ts }, &mut |_| {});
        }
    }

    /// Drains `receiver` into the tries until the stream closes. A `Lagged`
    /// receiver logs how many updates it dropped and resumes at the oldest
    /// retained message; the tries then simply carry the resulting gap until the
    /// affected prefixes are next announced or withdrawn.
    async fn run(&self, mut receiver: broadcast::Receiver<RisMessage>) {
        loop {
            match receiver.recv().await {
                Ok(message) => self.ingest(message),
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    metrics::counter!("slash0_route_table_dropped_messages_total")
                        .increment(dropped);
                    warn!(dropped, "route table ingest lagged behind RIS Live");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("RIS Live stream closed, route table ingest stopping");
                    break;
                }
            }
        }
    }

    fn tree_for(&self, version: IpVersion) -> &Mutex<ThickTree> {
        match version {
            IpVersion::V4 => &self.v4,
            IpVersion::V6 => &self.v6,
        }
    }

    /// Publishes the per-family trie tallies to the metrics recorder. Each trie
    /// maintains its own counts, so this is a lock plus a few field reads per
    /// family; call it on a periodic cadence rather than per update.
    pub fn record_metrics(&self) {
        for (version, ip_version) in [(IpVersion::V4, "v4"), (IpVersion::V6, "v6")] {
            let tree = self
                .tree_for(version)
                .lock()
                .expect("route table mutex poisoned");
            metrics::gauge!("slash0_route_table_total_nodes", "ipVersion" => ip_version)
                .set(tree.node_count() as f64);
            metrics::gauge!("slash0_route_table_announced_nodes", "ipVersion" => ip_version)
                .set(tree.announced_count() as f64);
            metrics::counter!("slash0_route_table_sweeps_total", "ipVersion" => ip_version)
                .absolute(tree.sweep_count() as u64);
            metrics::gauge!("slash0_route_table_slab_size", "ipVersion" => ip_version)
                .set(tree.slab.size() as f64);
            metrics::gauge!("slash0_route_table_slab_capacity", "ipVersion" => ip_version)
                .set(tree.slab.size_capacity() as f64);
        }
    }

    /// Reclaims tombstoned slots in both families, one family at a time so a
    /// sweep of one never blocks ingest of the other. Each walk runs in a span
    /// whose close event reports its duration and how many slots it reclaimed,
    /// so the sweep cadence can be evaluated against tree churn.
    ///
    /// Safe to run on any cadence server-side: the wire snapshot is a flat
    /// prefix list that never exposes slab indices, and [`subscribe`] clones
    /// under the same lock, so reclaimed slots can never be followed into an
    /// unrelated subtree the way the GPU's in-flight slab could.
    ///
    /// [`subscribe`]: RouteTable::subscribe
    pub fn sweep(&self) {
        for (version, ip_version) in [(IpVersion::V4, "v4"), (IpVersion::V6, "v6")] {
            let span = span!(
                Level::DEBUG,
                "sweep_tombstones",
                ip_version,
                reclaimed = field::Empty
            );
            let _guard = span.enter();
            let mut tree = self
                .tree_for(version)
                .lock()
                .expect("route table mutex poisoned");
            let before = tree.node_count();
            tree.sweep_tombstones(&mut |_| {});
            span.record("reclaimed", before - tree.node_count());
        }
    }

    /// Applies one message to the tries, then re-broadcasts it to subscribers.
    ///
    /// The re-broadcast happens *after* the trie mutation so a receiver handed
    /// out by [`subscribe`](Self::subscribe) can never start ahead of the
    /// snapshot it was paired with -- at worst it repeats the boundary message,
    /// which is harmless since applying an update is idempotent.
    fn ingest(&self, message: RisMessage) {
        self.apply_message(&message);
        // A send error only means no subscribers are attached right now.
        let _ = self.updates.send(message);
    }

    /// Applies one RIS Live message: every announced prefix is inserted and every
    /// withdrawn prefix is withdrawn, each routed to the trie for its family.
    /// Non-`UPDATE` messages and unparseable prefixes are skipped.
    fn apply_message(&self, message: &RisMessage) {
        let RisMessageBody::Update(update) = &message.body else {
            return;
        };
        let ts = Timestamp::from_sec(message.timestamp);

        let announced = update
            .announcements
            .iter()
            .flatten()
            .flat_map(|announcement| &announcement.prefixes);
        for cidr in announced {
            self.apply_prefix(cidr, "announced", |tree, prefix| {
                tree.insert(prefix, ts, ThickData::default(), &mut |_| {});
            });
        }

        for cidr in update.withdrawals.iter().flatten() {
            self.apply_prefix(cidr, "withdrawn", |tree, prefix| {
                tree.withdraw(prefix, ts, &mut |_| {});
            });
        }
    }

    /// Parses `cidr`, locks the trie for its family, and hands both to `apply`.
    /// Unparseable prefixes are logged and skipped rather than aborting the batch.
    fn apply_prefix(
        &self,
        cidr: &str,
        kind: &'static str,
        apply: impl FnOnce(&mut ThickTree, Prefix),
    ) {
        match Prefix::parse_cidr(cidr) {
            Ok((version, prefix)) => {
                let mut tree = self
                    .tree_for(version)
                    .lock()
                    .expect("route table mutex poisoned");
                apply(&mut tree, prefix);
            }
            Err(error) => warn!(%cidr, %error, "skipping unparseable {kind} prefix"),
        }
    }

    /// Returns an owned snapshot of the trie for `version` plus a receiver for the
    /// updates applied after it. The snapshot is decoupled from further ingest,
    /// and the receiver's stream resumes exactly where the snapshot ends.
    ///
    /// Blocks that family's ingest for the duration of the (cheap) slab clone.
    pub fn subscribe(
        &self,
        version: IpVersion,
    ) -> (Arc<ThickTree>, broadcast::Receiver<RisMessage>) {
        let tree = self
            .tree_for(version)
            .lock()
            .expect("route table mutex poisoned");
        let snapshot = Arc::new((*tree).clone());
        // Subscribe while still holding the lock: ingest re-broadcasts only after
        // applying, so nothing this family will stream can slip in before now.
        let updates = self.updates.subscribe();
        (snapshot, updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ris_client::messages::{Announcement, BgpUpdate};
    use slash0_core::prefix::Address;
    use slash0_core::slab::SlabRead;
    use std::path::PathBuf;

    fn message(timestamp: f64, body: RisMessageBody) -> RisMessage {
        RisMessage {
            timestamp,
            peer: "192.0.2.1".to_owned(),
            peer_asn: "64500".to_owned(),
            id: "test".to_owned(),
            host: "rrc00".to_owned(),
            raw: None,
            body,
        }
    }

    /// An `UPDATE` announcing every prefix in `announcements` (under one next
    /// hop) and withdrawing every prefix in `withdrawals`.
    fn update(announcements: &[&str], withdrawals: &[&str]) -> RisMessageBody {
        RisMessageBody::Update(BgpUpdate {
            announcements: (!announcements.is_empty()).then(|| {
                vec![Announcement {
                    next_hop: "192.0.2.1".to_owned(),
                    prefixes: announcements.iter().map(|s| s.to_string()).collect(),
                }]
            }),
            withdrawals: (!withdrawals.is_empty())
                .then(|| withdrawals.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        })
    }

    fn network_address(cidr: &str) -> Address {
        Prefix::parse_cidr(cidr).expect("valid CIDR").1.address()
    }

    /// Whether `tree` has `cidr` as a currently-announced exact-match prefix.
    fn is_announced(tree: &ThickTree, cidr: &str) -> bool {
        let (_, prefix) = Prefix::parse_cidr(cidr).expect("valid CIDR");
        match tree.lookup(prefix.address()) {
            Some(idx) => tree.slab.get(idx).prefix == prefix,
            None => false,
        }
    }

    fn snapshot(table: &RouteTable, version: IpVersion) -> Arc<ThickTree> {
        table.subscribe(version).0
    }

    #[test]
    fn routes_each_family_to_its_own_tree() {
        let table = RouteTable::new();
        table.ingest(message(1.0, update(&["192.0.2.0/24"], &[])));

        assert!(is_announced(
            &snapshot(&table, IpVersion::V4),
            "192.0.2.0/24"
        ));
        assert!(snapshot(&table, IpVersion::V6).root().is_none());

        table.ingest(message(1.0, update(&["2001:db8::/32"], &[])));
        assert!(is_announced(
            &snapshot(&table, IpVersion::V6),
            "2001:db8::/32"
        ));
    }

    #[test]
    fn withdraw_removes_the_prefix() {
        let table = RouteTable::new();
        table.ingest(message(1.0, update(&["192.0.2.0/24"], &[])));
        table.ingest(message(2.0, update(&[], &["192.0.2.0/24"])));

        assert!(
            snapshot(&table, IpVersion::V4)
                .lookup(network_address("192.0.2.0/24"))
                .is_none()
        );
    }

    #[test]
    fn non_update_messages_are_ignored() {
        let table = RouteTable::new();
        table.ingest(message(1.0, RisMessageBody::Keepalive));

        assert!(snapshot(&table, IpVersion::V4).root().is_none());
        assert!(snapshot(&table, IpVersion::V6).root().is_none());
    }

    #[test]
    fn one_message_routes_valid_prefixes_and_drops_malformed() {
        let table = RouteTable::new();
        table.ingest(message(
            1.0,
            update(&["192.0.2.0/24", "2001:db8::/32", "garbage/24"], &[]),
        ));

        assert!(is_announced(
            &snapshot(&table, IpVersion::V4),
            "192.0.2.0/24"
        ));
        assert!(is_announced(
            &snapshot(&table, IpVersion::V6),
            "2001:db8::/32"
        ));
    }

    #[test]
    fn derives_timestamp_from_message() {
        let table = RouteTable::new();
        table.ingest(message(1.5, update(&["192.0.2.0/24"], &[])));

        let tree = snapshot(&table, IpVersion::V4);
        let idx = tree.lookup(network_address("192.0.2.0/24")).unwrap();
        assert_eq!(
            tree.slab.get(idx).data.timestamp,
            Timestamp::from_millis(1500)
        );
    }

    #[test]
    fn empty_update_is_a_noop() {
        let table = RouteTable::new();
        table.ingest(message(1.0, update(&[], &[])));

        assert!(snapshot(&table, IpVersion::V4).root().is_none());
        assert!(snapshot(&table, IpVersion::V6).root().is_none());
    }

    #[test]
    fn reannounce_updates_timestamp() {
        let table = RouteTable::new();
        table.ingest(message(1.0, update(&["192.0.2.0/24"], &[])));
        table.ingest(message(2.0, update(&["192.0.2.0/24"], &[])));

        let tree = snapshot(&table, IpVersion::V4);
        let idx = tree.lookup(network_address("192.0.2.0/24")).unwrap();
        assert_eq!(
            tree.slab.get(idx).data.timestamp,
            Timestamp::from_millis(2000)
        );
    }

    #[test]
    fn withdraw_of_unannounced_prefix_leaves_tree_empty() {
        let table = RouteTable::new();
        table.ingest(message(1.0, update(&[], &["10.0.0.0/8"])));

        assert!(snapshot(&table, IpVersion::V4).root().is_none());
    }

    #[test]
    fn subscriber_stream_resumes_after_snapshot() {
        let table = RouteTable::new();
        table.ingest(message(1.0, update(&["192.0.2.0/24"], &[])));

        let (snapshot, mut updates) = table.subscribe(IpVersion::V4);

        // The snapshot reflects everything applied before the subscribe.
        assert!(is_announced(&snapshot, "192.0.2.0/24"));
        // The stream has nothing yet -- it begins where the snapshot ends.
        assert!(updates.try_recv().is_err());

        // A later update lands on the stream, but not in the already-taken snapshot.
        table.ingest(message(2.0, update(&["10.0.0.0/8"], &[])));

        let streamed = updates.try_recv().expect("stream carries the later update");
        assert!(matches!(streamed.body, RisMessageBody::Update(_)));
        assert!(snapshot.lookup(network_address("10.0.0.0/8")).is_none());
    }

    #[tokio::test]
    async fn run_drains_buffered_messages_then_stops_on_close() {
        let (tx, rx) = broadcast::channel(16);
        tx.send(message(1.0, update(&["192.0.2.0/24"], &[])))
            .unwrap();
        tx.send(message(2.0, update(&["2001:db8::/32"], &[])))
            .unwrap();
        drop(tx);

        // With the sender dropped, recv() yields both buffered messages in order
        // and then `Closed`, so run() drains everything and returns -- no sleeps.
        let table = RouteTable::new();
        table.run(rx).await;

        assert!(is_announced(
            &snapshot(&table, IpVersion::V4),
            "192.0.2.0/24"
        ));
        assert!(is_announced(
            &snapshot(&table, IpVersion::V6),
            "2001:db8::/32"
        ));
    }

    #[tokio::test]
    async fn run_survives_lag_and_keeps_applying() {
        // Overflow a small ring before draining so the first recv() lags.
        let (tx, rx) = broadcast::channel(2);
        for i in 0..5 {
            let prefix = format!("10.0.{i}.0/24");
            tx.send(message(1.0, update(&[prefix.as_str()], &[])))
                .unwrap();
        }
        drop(tx);

        let table = RouteTable::new();
        table.run(rx).await;

        // The most recent message is always retained, so it must have been
        // applied -- proving run() continued past the Lagged error.
        assert!(is_announced(
            &snapshot(&table, IpVersion::V4),
            "10.0.4.0/24"
        ));
    }

    #[test]
    fn add_routes_from_file() {
        let mut file_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        file_path.push("src/test_bview_file.mrt.gz");
        let reader = oneio::get_reader(file_path.to_str().unwrap()).unwrap();

        let parser = BgpkitParser::from_reader(reader);

        let route_table = RouteTable::new();
        route_table.add_routes_from(parser);

        let v4_len = route_table.v4.lock().unwrap().announced_count();
        let v6_len = route_table.v6.lock().unwrap().announced_count();

        assert_eq!(v4_len + v6_len, 100);
    }
}

use crate::node::{Node, NodeData, NodeFlags, NodeIdx};
use crate::prefix::{Address, MAX_PREFIX_LEN, Prefix};
#[cfg(feature = "thick")]
use crate::slab::VecSlab;
use crate::slab::{Slab, SlabRead};
#[cfg(feature = "thick")]
use crate::thick::ThickData;
#[cfg(feature = "thick")]
use crate::thin::ThinData;
use crate::timestamp::Timestamp;
use core::fmt::{Debug, Formatter};
use core::marker::PhantomData;

/// Path-compressed binary radix trie keyed on 128-bit prefixes, generic over
/// the per-node payload `D` and the slab storage `S`.
///
/// Nodes live in `S` and reference each other by `NodeIdx` (never pointers)
/// so the same slab can be memcpy'd wholesale into a GPU buffer for the
/// shader-side walk. Every mutation ([`insert`](Self::insert),
/// [`withdraw`](Self::withdraw), [`sweep_tombstones`](Self::sweep_tombstones))
/// takes a `dirty` callback that reports each slab index whose bytes changed,
/// giving callers exactly the set of `queue.write_buffer` uploads they need.
///
/// Timestamps propagate max-to-root on every announce and withdraw, so
/// `lookup` and downstream renderers can read "time since any descendant was
/// touched" from any interior node.
///
/// Cloning deep-copies the backing slab (requires `S: Clone`), yielding a tree
/// fully decoupled from the original -- the basis for handing out owned
/// snapshots.
#[apply(Serde!)]
#[derive(Clone)]
pub struct RadixTree<D: NodeData, S: SlabRead<Node<D>>> {
    pub slab: S,
    root: Option<NodeIdx>,
    /// Count of currently-announced nodes, maintained incrementally by the
    /// mutation API so the server can read it in O(1) rather than walking the
    /// trie. The total node count comes straight from the slab; only this
    /// announced subset needs its own tally. `u32` (not `usize`/`u64`) so the
    /// field stays shader-representable under `rust-gpu`.
    announced_count: u32,
    /// Number of [`sweep_tombstones`](Self::sweep_tombstones) passes run so far.
    sweep_count: u32,
    _phantom: PhantomData<D>,
}

/// Outcome of a single `insert_recursive` frame, returned back up the stack
/// so the parent frame can rewire its child pointer, merge its own timestamp,
/// and decide whether to keep propagating.
struct InsertOutcome {
    /// Node index the parent should now reference at this slot. Equals the
    /// input `subroot` unless the frame restructured (split / promotion).
    new_subroot: NodeIdx,
    /// The announced target node index -- returned all the way up unchanged.
    target: NodeIdx,
    /// Whether `ts` advanced this level's timestamp. Once this is false, the
    /// max-to-root invariant guarantees no ancestor above will advance either.
    ts_advanced: bool,
}

/// Outcome of a single `withdraw_recursive` frame.
enum WithdrawOutcome {
    NotFound,
    Withdrew {
        /// Whether `ts` advanced this level's timestamp; see `InsertOutcome`.
        ts_advanced: bool,
    },
}

// Read-only operations: available whenever the storage supports random reads
// (`SlabRead`). This is the surface a GPU-side implementation of the trie
// walk needs, and it's the only surface the shader can support since it
// cannot allocate.
impl<D: NodeData, S: SlabRead<Node<D>>> RadixTree<D, S> {
    /// Wraps the given slab as an empty tree (`root = None`). Callers with a
    /// mutation-capable slab use this to bootstrap; shader-side readers use
    /// [`with_root`](Self::with_root) instead, since they can't grow the
    /// tree and instead need to attach to a pre-populated slab + root.
    pub fn new(slab: S) -> Self {
        Self::with_root(slab, None)
    }

    /// Wraps a slab with an existing root index. This is the constructor
    /// used shader-side, where the slab bytes and the root are uploaded from
    /// the CPU and the shader only walks them.
    pub fn with_root(slab: S, root: Option<NodeIdx>) -> Self {
        Self {
            slab,
            root,
            announced_count: 0,
            sweep_count: 0,
            _phantom: PhantomData,
        }
    }

    /// Returns the slab index of the root node, or `None` if the tree is
    /// empty. Callers that memcpy the slab to the GPU need this to tell the
    /// shader where to start walking.
    pub fn root(&self) -> Option<NodeIdx> {
        self.root
    }

    /// Number of live nodes in the tree: announced prefixes plus the structural
    /// split nodes and not-yet-swept withdrawn nodes that hold the topology
    /// together. Read straight from the slab's live-allocation count, so it is
    /// accurate for any tree, including a [`with_root`](Self::with_root) view
    /// over an uploaded slab.
    pub fn node_count(&self) -> u32 {
        self.slab.len()
    }

    /// Number of currently-announced nodes, i.e. live BGP prefixes. Maintained
    /// incrementally by the mutation API, so it is only meaningful on a tree
    /// grown through [`insert`](Self::insert) / [`withdraw`](Self::withdraw),
    /// not on a [`with_root`](Self::with_root) view over an uploaded slab.
    pub fn announced_count(&self) -> u32 {
        self.announced_count
    }

    /// Number of [`sweep_tombstones`](Self::sweep_tombstones) passes performed
    /// on this tree, counting no-op passes. Carries the same maintenance caveat
    /// as [`announced_count`](Self::announced_count).
    pub fn sweep_count(&self) -> u32 {
        self.sweep_count
    }

    /// Longest-prefix-match lookup: returns the slab index of the deepest
    /// currently-announced node whose prefix covers `addr`, or `None` if no
    /// announced prefix covers it.
    ///
    /// Unannounced structural nodes (path-compression splits and withdrawn
    /// nodes pending sweep) are traversed but never returned.
    pub fn lookup(&self, addr: Address) -> Option<NodeIdx> {
        self.lookup_recursive(self.root?, addr)
    }

    fn lookup_recursive(&self, subroot: NodeIdx, addr: Address) -> Option<NodeIdx> {
        let node = self.slab.get(subroot);
        if !node.prefix.covers(addr) {
            return None;
        }
        // Prefer any deeper announced descendant over this level.
        let deeper = if node.prefix.len < MAX_PREFIX_LEN {
            let bit = addr.bit_at(node.prefix.len) as usize;
            node.children[bit].and_then(|c| self.lookup_recursive(c, addr))
        } else {
            None
        };
        deeper.or_else(|| node.is_announced().then_some(subroot))
    }
}

// Mutation operations: require the full `Slab` trait (`alloc` / `free` /
// `get_mut`). Unavailable on shader-side storage that only implements
// `SlabRead`.
impl<D: NodeData, S: Slab<Node<D>>> RadixTree<D, S> {
    /// Announces `prefix` with `data` at timestamp `ts`, returning the slab
    /// index of the target node.
    ///
    /// May restructure the tree by splitting an existing node or splicing a
    /// new ancestor above one. The target node's `ANNOUNCED` flag is set and
    /// `data.apply_announce(&incoming, ts)` is invoked on it. `ts` is then
    /// propagated max-to-root through the target's ancestors, stopping at the
    /// first ancestor whose timestamp already dominates.
    ///
    /// The `dirty` callback fires for every slab index whose bytes changed:
    /// the target, any structural nodes created by the split/promotion, any
    /// existing node whose child pointer was rewired, and any ancestor whose
    /// timestamp advanced. Each affected index is emitted exactly once.
    ///
    /// Panics if the underlying slab cannot allocate.
    pub fn insert(
        &mut self,
        prefix: Prefix,
        ts: Timestamp,
        incoming: D,
        dirty: &mut impl FnMut(NodeIdx),
    ) -> NodeIdx {
        match self.root {
            None => {
                let idx = self.alloc_announced_node(prefix, &incoming, ts);
                self.root = Some(idx);
                dirty(idx);
                idx
            }
            Some(root) => {
                let outcome = self.insert_recursive(root, prefix, &incoming, ts, dirty);
                self.root = Some(outcome.new_subroot);
                outcome.target
            }
        }
    }

    /// Recursive descent from `subroot`. See `InsertOutcome`.
    fn insert_recursive(
        &mut self,
        subroot: NodeIdx,
        prefix: Prefix,
        incoming: &D,
        ts: Timestamp,
        dirty: &mut impl FnMut(NodeIdx),
    ) -> InsertOutcome {
        let (node_prefix, node_children) = {
            let node = self.slab.get(subroot);
            (node.prefix, node.children)
        };

        // Exact match: re-announce at subroot.
        if node_prefix == prefix {
            let before = self.slab.get(subroot).data.timestamp();
            let was_announced = self.slab.get(subroot).is_announced();
            {
                let node = self.slab.get_mut(subroot);
                node.set_announced(true);
                node.data.apply_announce(incoming, ts);
            }
            // Re-announcing a tombstoned slot brings a node back to announced;
            // re-announcing a still-live prefix leaves the tally unchanged.
            if !was_announced {
                self.announced_count += 1;
            }
            dirty(subroot);
            return InsertOutcome {
                new_subroot: subroot,
                target: subroot,
                ts_advanced: ts > before,
            };
        }

        let common = prefix.common_prefix_len(&node_prefix);

        // Prefix extends subroot: descend into an existing child or graft a
        // new leaf.
        if common == node_prefix.len {
            let bit = prefix.bit_at(node_prefix.len) as usize;
            if let Some(child) = node_children[bit] {
                let sub = self.insert_recursive(child, prefix, incoming, ts, dirty);
                let child_changed = sub.new_subroot != child;
                if child_changed {
                    self.slab.get_mut(subroot).children[bit] = Some(sub.new_subroot);
                }
                let ts_advanced = sub.ts_advanced && {
                    let before = self.slab.get(subroot).data.timestamp();
                    self.slab.get_mut(subroot).data.merge_ancestor(ts);
                    self.slab.get(subroot).data.timestamp() != before
                };
                if child_changed || ts_advanced {
                    dirty(subroot);
                }
                return InsertOutcome {
                    new_subroot: subroot,
                    target: sub.target,
                    ts_advanced,
                };
            }
            // No child in that slot -- subroot gains a new leaf.
            let leaf_idx = self.alloc_announced_node(prefix, incoming, ts);
            self.slab.get_mut(subroot).children[bit] = Some(leaf_idx);
            dirty(leaf_idx);
            let before = self.slab.get(subroot).data.timestamp();
            self.slab.get_mut(subroot).data.merge_ancestor(ts);
            let after = self.slab.get(subroot).data.timestamp();
            dirty(subroot);
            return InsertOutcome {
                new_subroot: subroot,
                target: leaf_idx,
                ts_advanced: after != before,
            };
        }

        // Subroot extends prefix: prefix is an ancestor of subroot; splice a
        // new announced node above.
        if common == prefix.len {
            let node_bit = node_prefix.bit_at(prefix.len) as usize;
            let subtree_ts = self.slab.get(subroot).data.timestamp();
            let new_idx = self.alloc_announced_node(prefix, incoming, ts);
            {
                let n = self.slab.get_mut(new_idx);
                n.data.merge_ancestor(subtree_ts);
                n.children[node_bit] = Some(subroot);
            }
            dirty(new_idx);
            return InsertOutcome {
                new_subroot: new_idx,
                target: new_idx,
                ts_advanced: ts > subtree_ts,
            };
        }

        // Split: subroot and the new leaf share `common` bits; branch below.
        let subtree_ts = self.slab.get(subroot).data.timestamp();
        let split_prefix = Prefix::new(prefix.bits, common);
        let leaf_bit = prefix.bit_at(common) as usize;
        let node_bit = node_prefix.bit_at(common) as usize;
        debug_assert_ne!(leaf_bit, node_bit);
        let leaf_idx = self.alloc_announced_node(prefix, incoming, ts);
        let split_idx = self.alloc_split_node(split_prefix, subtree_ts);
        {
            let split = self.slab.get_mut(split_idx);
            split.children[leaf_bit] = Some(leaf_idx);
            split.children[node_bit] = Some(subroot);
            split.data.merge_ancestor(ts);
        }
        dirty(split_idx);
        dirty(leaf_idx);
        InsertOutcome {
            new_subroot: split_idx,
            target: leaf_idx,
            ts_advanced: ts > subtree_ts,
        }
    }

    /// Withdraws the exact-match `prefix` at timestamp `ts`, returning `true`
    /// iff the prefix was previously announced.
    ///
    /// Clears the target's `ANNOUNCED` flag and invokes
    /// `data.apply_withdraw(ts)` on it. `ts` is then propagated max-to-root
    /// through the target's ancestors. The target node's slab slot is *not*
    /// freed here -- reclamation is deferred to
    /// [`sweep_tombstones`](Self::sweep_tombstones) so the GPU's in-flight
    /// snapshot can't follow a recycled index into an unrelated subtree.
    ///
    /// Returns `false` (with no `dirty` emissions and no state change) when
    /// the prefix isn't in the tree or is present but not currently
    /// announced. `dirty` fires for the target and each ancestor whose
    /// timestamp advanced.
    pub fn withdraw(
        &mut self,
        prefix: Prefix,
        ts: Timestamp,
        dirty: &mut impl FnMut(NodeIdx),
    ) -> bool {
        let Some(root) = self.root else {
            return false;
        };
        matches!(
            self.withdraw_recursive(root, prefix, ts, dirty),
            WithdrawOutcome::Withdrew { .. }
        )
    }

    fn withdraw_recursive(
        &mut self,
        subroot: NodeIdx,
        prefix: Prefix,
        ts: Timestamp,
        dirty: &mut impl FnMut(NodeIdx),
    ) -> WithdrawOutcome {
        let (node_prefix, node_children, is_announced) = {
            let node = self.slab.get(subroot);
            (node.prefix, node.children, node.is_announced())
        };

        // Exact match: withdraw here if currently announced.
        if node_prefix == prefix {
            if !is_announced {
                return WithdrawOutcome::NotFound;
            }
            let before = self.slab.get(subroot).data.timestamp();
            {
                let node = self.slab.get_mut(subroot);
                node.set_announced(false);
                node.data.apply_withdraw(ts);
            }
            self.announced_count -= 1;
            dirty(subroot);
            return WithdrawOutcome::Withdrew {
                ts_advanced: ts > before,
            };
        }

        let common = prefix.common_prefix_len(&node_prefix);
        if common < node_prefix.len {
            return WithdrawOutcome::NotFound;
        }
        let bit = prefix.bit_at(node_prefix.len) as usize;
        let Some(child) = node_children[bit] else {
            return WithdrawOutcome::NotFound;
        };

        match self.withdraw_recursive(child, prefix, ts, dirty) {
            WithdrawOutcome::NotFound => WithdrawOutcome::NotFound,
            WithdrawOutcome::Withdrew {
                ts_advanced: sub_advanced,
            } => {
                let ts_advanced = sub_advanced && {
                    let before = self.slab.get(subroot).data.timestamp();
                    self.slab.get_mut(subroot).data.merge_ancestor(ts);
                    self.slab.get(subroot).data.timestamp() != before
                };
                if ts_advanced {
                    dirty(subroot);
                }
                WithdrawOutcome::Withdrew { ts_advanced }
            }
        }
    }

    fn alloc_announced_node(&mut self, prefix: Prefix, incoming: &D, ts: Timestamp) -> NodeIdx {
        let idx = self.slab.alloc().expect("slab full");
        let mut data = D::default();
        data.apply_announce(incoming, ts);
        *self.slab.get_mut(idx) = Node {
            children: [None, None],
            prefix,
            flags: NodeFlags::ANNOUNCED,
            data,
        };
        self.announced_count += 1;
        idx
    }

    fn alloc_split_node(&mut self, prefix: Prefix, subtree_ts: Timestamp) -> NodeIdx {
        let idx = self.slab.alloc().expect("slab full");
        let mut data = D::default();
        data.merge_ancestor(subtree_ts);
        *self.slab.get_mut(idx) = Node {
            children: [None, None],
            prefix,
            flags: NodeFlags::empty(),
            data,
        };
        idx
    }

    /// Reclaims slab slots that are no longer contributing to the tree.
    /// Meant to run at a frame boundary -- never during a frame -- so the GPU's
    /// current-frame slab snapshot cannot follow a recycled index into an
    /// unrelated subtree.
    ///
    /// Post-order pass: a non-announced node is collapsed if it has zero or
    /// one child (the child, if any, is spliced up into the parent's slot),
    /// and kept if it has two (it remains a pure topology split). This
    /// treats withdrawn-and-tombstoned nodes and never-announced split nodes
    /// uniformly, so a split that becomes redundant after a subtree is
    /// swept away also gets collapsed in the same pass.
    ///
    /// `dirty` fires for each parent whose child pointer changed; the freed
    /// slot itself is not emitted (nothing in the live tree references it).
    pub fn sweep_tombstones(&mut self, dirty: &mut impl FnMut(NodeIdx)) {
        self.sweep_count += 1;
        let Some(root) = self.root else { return };
        self.sweep_recursive(root, None, dirty);
    }

    // Freed slots themselves are never dirtied: the GPU is kept away from
    // them by dirtying the parent whose child pointer just cleared. There is
    // one remaining source of wasted uploads under sweep: when a chain of N
    // nested nodes all collapse in one pass, each intermediate is dirtied by
    // its (freed) child's set_parent_slot call and then freed itself moments
    // later. That's N-1 wasted PCIe writes for a depth-N collapse. Cheap to
    // fix by buffering dirties inside sweep_tombstones and filtering out
    // indices that were freed by the end of the pass; deferred because chain
    // collapses are rare at frame-boundary cadence.

    fn sweep_recursive(
        &mut self,
        idx: NodeIdx,
        parent_slot: Option<(NodeIdx, usize)>,
        dirty: &mut impl FnMut(NodeIdx),
    ) {
        let (child0, child1) = {
            let node = self.slab.get(idx);
            (node.children[0], node.children[1])
        };
        if let Some(child) = child0 {
            self.sweep_recursive(child, Some((idx, 0)), dirty);
        }
        if let Some(child) = child1 {
            self.sweep_recursive(child, Some((idx, 1)), dirty);
        }

        let (l, r, is_announced) = {
            let node = self.slab.get(idx);
            (node.children[0], node.children[1], node.is_announced())
        };
        if is_announced {
            return;
        }
        match (l, r) {
            (Some(_), Some(_)) => {}
            (None, None) => {
                self.set_parent_slot(parent_slot, None, dirty);
                self.slab.free(idx);
            }
            (Some(child), None) | (None, Some(child)) => {
                self.set_parent_slot(parent_slot, Some(child), dirty);
                self.slab.free(idx);
            }
        }
    }

    fn set_parent_slot(
        &mut self,
        parent_slot: Option<(NodeIdx, usize)>,
        value: Option<NodeIdx>,
        dirty: &mut impl FnMut(NodeIdx),
    ) {
        match parent_slot {
            None => self.root = value,
            Some((parent, slot)) => {
                self.slab.get_mut(parent).children[slot] = value;
                dirty(parent);
            }
        }
    }
}

impl<D: Debug + NodeData, S: SlabRead<Node<D>>> Debug for RadixTree<D, S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RadixTree")
            .field("root_idx", &self.root)
            .field("root", &self.root.map(|i| self.slab.get(i)))
            .field("announced_count", &self.announced_count)
            .field("sweep_count", &self.sweep_count)
            .finish()
    }
}

#[cfg(feature = "thick")]
impl RadixTree<ThinData, VecSlab<Node<ThinData>>> {
    /// Create a thin tree out of a thick tree. The thick tree is basically cloned, the resulting
    /// thin tree is a fully owned copy of the data with no references back to the thick tree.
    ///
    /// Note: The sweep_count is reset to 0 on the thin tree.
    pub fn from_thick_tree(thick_tree: &RadixTree<ThickData, VecSlab<Node<ThickData>>>) -> Self {
        let thin_slab = VecSlab::with_entries_mapped_from(&thick_tree.slab, |thick_node| Node {
            children: thick_node.children,
            prefix: thick_node.prefix,
            flags: thick_node.flags,
            data: ThinData {
                timestamp: thick_node.data.timestamp,
            },
        });

        RadixTree {
            slab: thin_slab,
            root: thick_tree.root,
            announced_count: thick_tree.announced_count,
            sweep_count: 0,
            _phantom: PhantomData,
        }
    }
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use super::*;
    use crate::slab::{SlabRead, VecSlab};
    use crate::thin::ThinData;
    use alloc::collections::{BTreeMap, BTreeSet};
    use alloc::vec::Vec;

    type Tree = RadixTree<ThinData, VecSlab<Node<ThinData>>>;

    fn new_tree() -> Tree {
        RadixTree::new(VecSlab::new())
    }

    fn addr(dotted: &str) -> Address {
        let ip: core::net::Ipv4Addr = dotted.parse().expect("valid IPv4 dotted-decimal");
        Address([u32::from_be_bytes(ip.octets()), 0, 0, 0])
    }

    fn v4(cidr: &str) -> Prefix {
        let (addr_str, len_str) = cidr.split_once('/').expect("CIDR notation like 10.0.0.0/8");
        let bits = addr(addr_str).0;
        let len: u32 = len_str.parse().expect("valid prefix length");
        Prefix::new(bits, len)
    }

    /// Snapshot every node reachable from the root, keyed by slab index.
    fn snapshot_reachable(tree: &Tree) -> BTreeMap<NodeIdx, Node<ThinData>> {
        fn collect(tree: &Tree, idx: NodeIdx, out: &mut BTreeMap<NodeIdx, Node<ThinData>>) {
            if out.contains_key(&idx) {
                return;
            }
            let node = *tree.slab.get(idx);
            out.insert(idx, node);
            for child in node.children.iter().flatten() {
                collect(tree, *child, out);
            }
        }
        let mut out = BTreeMap::new();
        if let Some(root) = tree.root() {
            collect(tree, root, &mut out);
        }
        out
    }

    /// Runs a tree mutation and asserts that every reachable slab entry whose
    /// bytes changed (or that appeared new) was reported via the dirty
    /// callback. Over-reporting (dirtying unchanged nodes) is permitted.
    ///
    /// `op` receives the tree and a `Vec` sink; push node indices into the
    /// sink from your own callback passed to `insert` / `withdraw` / etc.
    fn run_and_check_dirty<R>(
        tree: &mut Tree,
        op: impl FnOnce(&mut Tree, &mut Vec<NodeIdx>) -> R,
    ) -> R {
        let before = snapshot_reachable(tree);
        let mut dirty: Vec<NodeIdx> = Vec::new();
        let result = op(tree, &mut dirty);
        let after = snapshot_reachable(tree);
        let dirty_set: BTreeSet<NodeIdx> = dirty.iter().copied().collect();

        // Direction 1: every actual change must be reported.
        for (idx, after_node) in &after {
            let changed = match before.get(idx) {
                None => true,
                Some(before_node) => before_node != after_node,
            };
            assert!(
                !changed || dirty_set.contains(idx),
                "slab index {} changed but was not reported via dirty callback \
                 (dirty set = {:?}, before = {:?}, after = {:?})",
                idx.get(),
                dirty_set,
                before.get(idx),
                after_node,
            );
        }

        // Direction 2: every emission must correspond to an actual change.
        // An emission is legitimate iff the slot's bytes differ pre vs post,
        // or the slot was newly allocated, or the slot was freed. A
        // wasteful emission (dirty for a live, unchanged slot) is a
        // regression that costs a GPU upload of unchanged bytes.
        for &idx in &dirty_set {
            // (Some, None) freed, (None, Some) newly allocated, (None, None)
            // allocated-and-freed in one mutation -- all legitimate. Only
            // (Some, Some) with identical bytes is wasteful.
            if let (Some(b), Some(a)) = (before.get(&idx), after.get(&idx)) {
                assert!(
                    b != a,
                    "slab index {} was dirtied but its bytes did not change \
                     (before == after == {:?})",
                    idx.get(),
                    a,
                );
            }
        }

        // Direction 3: no duplicate emissions. Two dirties for the same slot
        // cost two GPU uploads of identical bytes.
        assert_eq!(
            dirty.len(),
            dirty_set.len(),
            "duplicate dirty emissions ({:?})",
            dirty,
        );

        result
    }

    fn ins(tree: &mut Tree, p: Prefix, ts_ms: u64) -> NodeIdx {
        run_and_check_dirty(tree, |t, d| {
            t.insert(
                p,
                Timestamp::from_millis(ts_ms),
                ThinData::default(),
                &mut |i| d.push(i),
            )
        })
    }

    fn wdw(tree: &mut Tree, p: Prefix, ts_ms: u64) -> bool {
        run_and_check_dirty(tree, |t, d| {
            t.withdraw(p, Timestamp::from_millis(ts_ms), &mut |i| d.push(i))
        })
    }

    fn swp(tree: &mut Tree) {
        run_and_check_dirty(tree, |t, d| {
            t.sweep_tombstones(&mut |i| d.push(i));
        });
    }

    #[test]
    fn empty_tree_lookup_returns_none() {
        let tree = new_tree();
        assert_eq!(tree.lookup(addr("10.0.0.1")), None);
    }

    #[test]
    fn empty_tree_withdraw_returns_false() {
        let mut tree = new_tree();
        assert!(!wdw(&mut tree, v4("10.0.0.0/8"), 1));
    }

    #[test]
    fn empty_tree_sweep_is_noop() {
        let mut tree = new_tree();
        swp(&mut tree);
        assert!(tree.root().is_none());
    }

    #[test]
    fn insert_into_empty_tree_then_lookup() {
        let mut tree = new_tree();
        let idx = ins(&mut tree, v4("10.0.0.0/8"), 100);
        assert_eq!(tree.lookup(addr("10.1.2.3")), Some(idx));
        assert_eq!(tree.root(), Some(idx));
    }

    #[test]
    fn insert_two_disjoint_creates_split() {
        let mut tree = new_tree();
        let a = ins(&mut tree, v4("0.0.0.0/1"), 100);
        let b = ins(&mut tree, v4("128.0.0.0/1"), 200);
        let root = tree.root().unwrap();
        assert_ne!(root, a);
        assert_ne!(root, b);
        let root_node = tree.slab.get(root);
        assert_eq!(root_node.prefix.len, 0);
        assert!(!root_node.is_announced());
        assert_eq!(root_node.child_count(), 2);
        assert_eq!(tree.lookup(addr("1.0.0.0")), Some(a));
        assert_eq!(tree.lookup(addr("200.0.0.0")), Some(b));
    }

    #[test]
    fn split_node_does_not_leak_to_uncovered_addresses() {
        // Two /8s with disjoint first bits force a split at /0. An address
        // that neither /8 covers must return None, not inherit the split.
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        ins(&mut tree, v4("200.0.0.0/8"), 100);
        assert!(!tree.slab.get(tree.root().unwrap()).is_announced());
        assert_eq!(tree.lookup(addr("192.0.2.1")), None);
    }

    #[test]
    fn insert_extend_then_ancestor_promotion() {
        let mut tree = new_tree();
        let long = ins(&mut tree, v4("10.0.0.0/16"), 100);
        let short = ins(&mut tree, v4("10.0.0.0/8"), 200);
        assert_eq!(tree.root(), Some(short));
        assert!(tree.slab.get(short).is_announced());
        assert!(tree.slab.get(long).is_announced());
        assert_eq!(tree.lookup(addr("10.0.0.1")), Some(long));
        assert_eq!(tree.lookup(addr("10.128.0.1")), Some(short));
    }

    #[test]
    fn insert_extend_existing_intermediate() {
        let mut tree = new_tree();
        let a = ins(&mut tree, v4("10.0.0.0/8"), 100);
        let b = ins(&mut tree, v4("10.128.0.0/9"), 200);
        let c = ins(&mut tree, v4("10.0.0.0/9"), 300);
        assert_eq!(tree.lookup(addr("10.200.0.0")), Some(b));
        assert_eq!(tree.lookup(addr("10.1.0.0")), Some(c));
        assert_eq!(tree.lookup(addr("10.0.0.0")), Some(c));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn lpm_finds_deepest_announced() {
        let mut tree = new_tree();
        let eight = ins(&mut tree, v4("10.0.0.0/8"), 100);
        let sixteen = ins(&mut tree, v4("10.1.0.0/16"), 100);
        assert_eq!(tree.lookup(addr("10.1.2.3")), Some(sixteen));
        assert_eq!(tree.lookup(addr("10.2.2.3")), Some(eight));
    }

    #[test]
    fn lookup_no_match_returns_none_in_populated_tree() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        assert_eq!(tree.lookup(addr("192.0.2.1")), None);
    }

    #[test]
    fn withdrawn_hidden_from_lookup_before_sweep() {
        let mut tree = new_tree();
        let a = ins(&mut tree, v4("10.0.0.0/8"), 100);
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        assert_eq!(tree.lookup(addr("10.1.2.3")), None);
        assert!(!tree.slab.get(a).is_announced());
    }

    #[test]
    fn double_withdraw_returns_false() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        assert!(!wdw(&mut tree, v4("10.0.0.0/8"), 300));
    }

    #[test]
    fn timestamp_propagates_to_root() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        ins(&mut tree, v4("128.0.0.0/8"), 500);
        let root = tree.root().unwrap();
        assert_eq!(
            tree.slab.get(root).data.timestamp(),
            Timestamp::from_millis(500),
        );
    }

    #[test]
    fn timestamp_propagation_monotonic() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 500);
        ins(&mut tree, v4("128.0.0.0/8"), 200);
        let root = tree.root().unwrap();
        assert_eq!(
            tree.slab.get(root).data.timestamp(),
            Timestamp::from_millis(500),
        );
    }

    #[test]
    fn reannounce_reuses_slot() {
        let mut tree = new_tree();
        let a = ins(&mut tree, v4("10.0.0.0/8"), 100);
        let len_before = tree.slab.len();
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        let b = ins(&mut tree, v4("10.0.0.0/8"), 300);
        assert_eq!(a, b, "re-announce should reuse the slot");
        assert_eq!(tree.slab.len(), len_before);
        assert!(tree.slab.get(a).is_announced());
    }

    #[test]
    fn sweep_collapses_withdrawn_chain() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        ins(&mut tree, v4("10.0.0.0/16"), 100);
        ins(&mut tree, v4("10.0.0.0/24"), 100);
        assert!(wdw(&mut tree, v4("10.0.0.0/24"), 200));
        assert!(wdw(&mut tree, v4("10.0.0.0/16"), 200));
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        swp(&mut tree);
        assert_eq!(tree.root(), None);
    }

    #[test]
    fn sweep_collapses_degenerate_split() {
        let mut tree = new_tree();
        ins(&mut tree, v4("0.0.0.0/8"), 100);
        let b = ins(&mut tree, v4("128.0.0.0/8"), 100);
        let root_before = tree.root().unwrap();
        assert!(!tree.slab.get(root_before).is_announced());
        assert!(wdw(&mut tree, v4("0.0.0.0/8"), 200));
        swp(&mut tree);
        assert_eq!(
            tree.root(),
            Some(b),
            "root should have collapsed to the remaining announced leaf",
        );
    }

    #[test]
    fn sweep_idempotent() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        ins(&mut tree, v4("11.0.0.0/8"), 100);
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        swp(&mut tree);
        let root_after_first = tree.root();
        let len_after_first = tree.slab.len();
        swp(&mut tree);
        assert_eq!(tree.root(), root_after_first);
        assert_eq!(tree.slab.len(), len_after_first);
    }

    #[test]
    fn counts_start_at_zero() {
        let tree = new_tree();
        assert_eq!(tree.node_count(), 0);
        assert_eq!(tree.announced_count(), 0);
        assert_eq!(tree.sweep_count(), 0);
    }

    #[test]
    fn insert_tracks_node_and_announced_counts() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        assert_eq!(tree.node_count(), 1);
        assert_eq!(tree.announced_count(), 1);

        // A disjoint prefix forces a structural split: one announced leaf plus
        // one non-announced split node.
        ins(&mut tree, v4("128.0.0.0/8"), 100);
        assert_eq!(tree.node_count(), 3);
        assert_eq!(tree.announced_count(), 2);
    }

    #[test]
    fn reannounce_of_live_prefix_does_not_double_count() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        ins(&mut tree, v4("10.0.0.0/8"), 200);
        assert_eq!(tree.node_count(), 1);
        assert_eq!(tree.announced_count(), 1);
    }

    #[test]
    fn withdraw_drops_announced_but_keeps_node_until_sweep() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        assert_eq!(tree.announced_count(), 0);
        assert_eq!(tree.node_count(), 1, "tombstone remains until swept");
    }

    #[test]
    fn double_withdraw_decrements_announced_once() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        assert!(!wdw(&mut tree, v4("10.0.0.0/8"), 300));
        assert_eq!(tree.announced_count(), 0);
    }

    #[test]
    fn reannounce_after_withdraw_recounts_announced() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        ins(&mut tree, v4("10.0.0.0/8"), 300);
        assert_eq!(tree.announced_count(), 1);
        assert_eq!(tree.node_count(), 1, "the tombstoned slot is reused");
    }

    #[test]
    fn announcing_an_existing_structural_node_counts_announced_only() {
        let mut tree = new_tree();
        // Two /9s under a common /8 leave a structural, never-announced split
        // node sitting at 10.0.0.0/8.
        ins(&mut tree, v4("10.0.0.0/9"), 100);
        ins(&mut tree, v4("10.128.0.0/9"), 100);
        assert_eq!(tree.announced_count(), 2);
        assert_eq!(tree.node_count(), 3);

        // Announcing that exact /8 lights up the existing split node: no new
        // slot is allocated, but the announced tally rises.
        ins(&mut tree, v4("10.0.0.0/8"), 200);
        assert_eq!(
            tree.node_count(),
            3,
            "no new node should have been allocated"
        );
        assert_eq!(tree.announced_count(), 3);
    }

    #[test]
    fn sweep_reclaims_tombstoned_nodes_from_node_count() {
        let mut tree = new_tree();
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        ins(&mut tree, v4("128.0.0.0/8"), 100);
        assert_eq!(tree.node_count(), 3);
        assert!(wdw(&mut tree, v4("10.0.0.0/8"), 200));
        swp(&mut tree);
        // The withdrawn leaf is freed and the now-degenerate split collapses,
        // leaving only the surviving announced /8.
        assert_eq!(tree.node_count(), 1);
        assert_eq!(tree.announced_count(), 1);
    }

    #[test]
    fn sweep_count_increments_every_call_including_noops() {
        let mut tree = new_tree();
        swp(&mut tree);
        assert_eq!(tree.sweep_count(), 1, "an empty-tree sweep still counts");
        ins(&mut tree, v4("10.0.0.0/8"), 100);
        swp(&mut tree);
        assert_eq!(tree.sweep_count(), 2);
    }

    #[test]
    fn ipv6_lookup() {
        let mut tree = new_tree();
        let sixty_four = ins(
            &mut tree,
            Prefix::new([0x2001_0db8, 0x1234_0000, 0, 0], 64),
            100,
        );
        let one_twenty_eight = ins(
            &mut tree,
            Prefix::new([0x2001_0db8, 0x1234_0000, 0, 1], 128),
            100,
        );
        assert_eq!(
            tree.lookup(Address([0x2001_0db8, 0x1234_0000, 0, 1])),
            Some(one_twenty_eight)
        );
        assert_eq!(
            tree.lookup(Address([0x2001_0db8, 0x1234_0000, 0, 0xDEAD_BEEF])),
            Some(sixty_four)
        );
        assert_eq!(tree.lookup(Address([0x2001_0db8, 0xFFFF_0000, 0, 0])), None);
    }

    fn lcg(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 32) as u32
    }

    fn naive_lpm(prefixes: &[Prefix], addr: Address) -> Option<Prefix> {
        let mut best: Option<Prefix> = None;
        for &p in prefixes {
            if p.covers(addr) {
                let better = match best {
                    None => true,
                    Some(b) => p.len > b.len,
                };
                if better {
                    best = Some(p);
                }
            }
        }
        best
    }

    #[test]
    fn oracle_lpm_matches_naive_scan() {
        let mut tree = new_tree();
        let mut naive: Vec<Prefix> = Vec::new();
        let mut seed = 0xDEAD_BEEF_CAFE_BABEu64;

        for _ in 0..500 {
            let word0 = lcg(&mut seed);
            let prefix_len = 8 + (lcg(&mut seed) % 25);
            let prefix = Prefix::new([word0, 0, 0, 0], prefix_len);
            ins(&mut tree, prefix, 100);
            naive.retain(|p| *p != prefix);
            naive.push(prefix);
        }

        for _ in 0..500 {
            let addr = Address([lcg(&mut seed), 0, 0, 0]);
            let expected = naive_lpm(&naive, addr);
            let actual = tree.lookup(addr).map(|idx| tree.slab.get(idx).prefix);
            assert_eq!(actual, expected, "addr {:?}", addr);
        }
    }
}

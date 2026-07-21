use crate::node::{Node, NodeData, NodeFlags, NodeIdx};
use crate::prefix::{MAX_PREFIX_LEN, Prefix};
use crate::slab::Slab;
use crate::timestamp::Timestamp;
use core::marker::PhantomData;

pub struct RadixTree<D: NodeData, S: Slab<Node<D>>> {
    pub slab: S,
    root: Option<NodeIdx>,
    _phantom: PhantomData<D>,
}

impl<D: NodeData, S: Slab<Node<D>>> RadixTree<D, S> {
    pub fn new(slab: S) -> Self {
        Self {
            slab,
            root: None,
            _phantom: PhantomData,
        }
    }

    pub fn root(&self) -> Option<NodeIdx> {
        self.root
    }

    pub fn insert(
        &mut self,
        prefix: Prefix,
        ts: Timestamp,
        incoming: D,
        dirty: &mut impl FnMut(NodeIdx),
    ) -> NodeIdx {
        let Some(mut current) = self.root else {
            let idx = self.alloc_announced_node(prefix, &incoming, ts);
            self.root = Some(idx);
            dirty(idx);
            return idx;
        };

        let mut path: [Option<NodeIdx>; MAX_PREFIX_LEN as usize + 1] =
            [None; MAX_PREFIX_LEN as usize + 1];
        let mut path_len: usize = 0;
        let mut parent: Option<(NodeIdx, usize)> = None;

        loop {
            let (node_prefix, node_children) = {
                let node = self.slab.get(current);
                (node.prefix, node.children)
            };
            let common = prefix.common_prefix_len(&node_prefix);

            if common == node_prefix.len && common == prefix.len {
                {
                    let node = self.slab.get_mut(current);
                    node.set_announced(true);
                    node.data.apply_announce(&incoming, ts);
                }
                dirty(current);
                self.propagate_up(&path[..path_len], ts, dirty);
                return current;
            }

            if common == node_prefix.len {
                let bit = prefix.bit_at(node_prefix.len) as usize;
                if let Some(child) = node_children[bit] {
                    path[path_len] = Some(current);
                    path_len += 1;
                    parent = Some((current, bit));
                    current = child;
                    continue;
                }
                let leaf_idx = self.alloc_announced_node(prefix, &incoming, ts);
                self.slab.get_mut(current).children[bit] = Some(leaf_idx);
                dirty(leaf_idx);
                self.dirty_and_propagate(current, ts, &path[..path_len], dirty);
                return leaf_idx;
            }

            if common == prefix.len {
                let node_bit = node_prefix.bit_at(prefix.len) as usize;
                let current_ts = self.slab.get(current).data.timestamp();
                let new_idx = self.alloc_announced_node(prefix, &incoming, ts);
                {
                    let n = self.slab.get_mut(new_idx);
                    n.data.merge_ancestor(current_ts);
                    n.children[node_bit] = Some(current);
                }
                dirty(new_idx);
                // Handle the parent (pointer edit + possible ts advance) once.
                // current itself is unchanged.
                match parent {
                    None => self.root = Some(new_idx),
                    Some((parent_idx, slot)) => {
                        self.slab.get_mut(parent_idx).children[slot] = Some(new_idx);
                        debug_assert_eq!(path[path_len - 1], Some(parent_idx));
                        path_len -= 1;
                        self.dirty_and_propagate(parent_idx, ts, &path[..path_len], dirty);
                    }
                }
                return new_idx;
            }

            // Split: `current` and the new leaf share `common` bits; branch below there.
            let split_prefix = Prefix::new(prefix.bits, common);
            let leaf_bit = prefix.bit_at(common) as usize;
            let node_bit = node_prefix.bit_at(common) as usize;
            debug_assert_ne!(leaf_bit, node_bit);
            let current_ts = self.slab.get(current).data.timestamp();

            let leaf_idx = self.alloc_announced_node(prefix, &incoming, ts);
            let split_idx = self.alloc_split_node(split_prefix, current_ts);
            {
                let split = self.slab.get_mut(split_idx);
                split.children[leaf_bit] = Some(leaf_idx);
                split.children[node_bit] = Some(current);
                split.data.merge_ancestor(ts);
            }
            dirty(split_idx);
            dirty(leaf_idx);
            match parent {
                None => self.root = Some(split_idx),
                Some((parent_idx, slot)) => {
                    self.slab.get_mut(parent_idx).children[slot] = Some(split_idx);
                    debug_assert_eq!(path[path_len - 1], Some(parent_idx));
                    path_len -= 1;
                    self.dirty_and_propagate(parent_idx, ts, &path[..path_len], dirty);
                }
            }
            return leaf_idx;
        }
    }

    pub fn withdraw(
        &mut self,
        prefix: Prefix,
        ts: Timestamp,
        dirty: &mut impl FnMut(NodeIdx),
    ) -> bool {
        let Some(mut current) = self.root else {
            return false;
        };
        let mut path: [Option<NodeIdx>; MAX_PREFIX_LEN as usize + 1] =
            [None; MAX_PREFIX_LEN as usize + 1];
        let mut path_len: usize = 0;

        loop {
            let (node_prefix, node_children, is_announced) = {
                let node = self.slab.get(current);
                (node.prefix, node.children, node.is_announced())
            };

            if node_prefix == prefix {
                if !is_announced {
                    return false;
                }
                {
                    let node = self.slab.get_mut(current);
                    node.set_announced(false);
                    node.data.apply_withdraw(ts);
                }
                dirty(current);
                self.propagate_up(&path[..path_len], ts, dirty);
                return true;
            }

            let common = prefix.common_prefix_len(&node_prefix);
            if common < node_prefix.len {
                return false;
            }
            let bit = prefix.bit_at(node_prefix.len) as usize;
            match node_children[bit] {
                None => return false,
                Some(child) => {
                    path[path_len] = Some(current);
                    path_len += 1;
                    current = child;
                }
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

    pub fn lookup(&self, addr: [u32; 4]) -> Option<NodeIdx> {
        let mut current = self.root?;
        let mut best: Option<NodeIdx> = None;

        loop {
            let node = self.slab.get(current);
            if !node.prefix.covers(&addr) {
                return best;
            }
            if node.is_announced() {
                best = Some(current);
            }
            if node.prefix.len == MAX_PREFIX_LEN {
                return best;
            }
            let bit = crate::prefix::bit_at(&addr, node.prefix.len) as usize;
            match node.children[bit] {
                None => return best,
                Some(child) => current = child,
            }
        }
    }

    pub fn sweep_tombstones(&mut self, dirty: &mut impl FnMut(NodeIdx)) {
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

    fn propagate_up(
        &mut self,
        path: &[Option<NodeIdx>],
        ts: Timestamp,
        dirty: &mut impl FnMut(NodeIdx),
    ) {
        for &opt_idx in path.iter().rev() {
            let idx = opt_idx.expect("path entries are always Some");
            let before = self.slab.get(idx).data.timestamp();
            self.slab.get_mut(idx).data.merge_ancestor(ts);
            let after = self.slab.get(idx).data.timestamp();
            if after != before {
                dirty(idx);
            } else {
                break;
            }
        }
    }

    /// Merge `ts` into `target`, dirty `target` unconditionally, and only
    /// propagate further into `ancestors` (strictly above `target`) if
    /// `target`'s timestamp actually advanced. Ensures `target` is emitted
    /// exactly once even when its bytes changed for a non-timestamp reason
    /// (e.g., a children-pointer edit).
    fn dirty_and_propagate(
        &mut self,
        target: NodeIdx,
        ts: Timestamp,
        ancestors: &[Option<NodeIdx>],
        dirty: &mut impl FnMut(NodeIdx),
    ) {
        let before = self.slab.get(target).data.timestamp();
        self.slab.get_mut(target).data.merge_ancestor(ts);
        let after = self.slab.get(target).data.timestamp();
        dirty(target);
        if after != before {
            self.propagate_up(ancestors, ts, dirty);
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

    fn addr(dotted: &str) -> [u32; 4] {
        let ip: core::net::Ipv4Addr = dotted.parse().expect("valid IPv4 dotted-decimal");
        [u32::from_be_bytes(ip.octets()), 0, 0, 0]
    }

    fn v4(cidr: &str) -> Prefix {
        let (addr_str, len_str) = cidr.split_once('/').expect("CIDR notation like 10.0.0.0/8");
        let bits = addr(addr_str);
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
            // allocated-and-freed in one mutation — all legitimate. Only
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
            tree.lookup([0x2001_0db8, 0x1234_0000, 0, 1]),
            Some(one_twenty_eight)
        );
        assert_eq!(
            tree.lookup([0x2001_0db8, 0x1234_0000, 0, 0xDEAD_BEEF]),
            Some(sixty_four)
        );
        assert_eq!(tree.lookup([0x2001_0db8, 0xFFFF_0000, 0, 0]), None);
    }

    fn lcg(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 32) as u32
    }

    fn naive_lpm(prefixes: &[Prefix], addr: [u32; 4]) -> Option<Prefix> {
        let mut best: Option<Prefix> = None;
        for &p in prefixes {
            if p.covers(&addr) {
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
            let addr = [lcg(&mut seed), 0, 0, 0];
            let expected = naive_lpm(&naive, addr);
            let actual = tree.lookup(addr).map(|idx| tree.slab.get(idx).prefix);
            assert_eq!(actual, expected, "addr {:?}", addr);
        }
    }
}

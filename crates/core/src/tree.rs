use crate::node::{Node, NodeData, NodeFlags, NodeIdx};
use crate::prefix::{MAX_PREFIX_LEN, bit_at, common_prefix_len, mask_prefix};
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
        prefix: [u32; 4],
        prefix_len: u32,
        ts: Timestamp,
        incoming: D,
        dirty: &mut impl FnMut(NodeIdx),
    ) -> NodeIdx {
        assert!(prefix_len <= MAX_PREFIX_LEN);
        let prefix = mask_prefix(prefix, prefix_len);

        let Some(mut current) = self.root else {
            let idx = self.alloc_announced_node(prefix, prefix_len, &incoming, ts);
            self.root = Some(idx);
            dirty(idx);
            return idx;
        };

        let mut path: [Option<NodeIdx>; MAX_PREFIX_LEN as usize + 1] =
            [None; MAX_PREFIX_LEN as usize + 1];
        let mut path_len: usize = 0;
        let mut parent: Option<(NodeIdx, usize)> = None;

        loop {
            let (node_prefix, node_prefix_len, node_children) = {
                let node = self.slab.get(current);
                (node.prefix, node.prefix_len, node.children)
            };
            let common = common_prefix_len(&prefix, prefix_len, &node_prefix, node_prefix_len);

            if common == node_prefix_len && common == prefix_len {
                {
                    let node = self.slab.get_mut(current);
                    node.set_announced(true);
                    node.data.apply_announce(&incoming, ts);
                }
                dirty(current);
                self.propagate_up(&path[..path_len], ts, dirty);
                return current;
            }

            if common == node_prefix_len {
                let bit = bit_at(&prefix, node_prefix_len) as usize;
                if let Some(child) = node_children[bit] {
                    path[path_len] = Some(current);
                    path_len += 1;
                    parent = Some((current, bit));
                    current = child;
                    continue;
                }
                let leaf_idx = self.alloc_announced_node(prefix, prefix_len, &incoming, ts);
                self.slab.get_mut(current).children[bit] = Some(leaf_idx);
                dirty(current);
                dirty(leaf_idx);
                path[path_len] = Some(current);
                path_len += 1;
                self.propagate_up(&path[..path_len], ts, dirty);
                return leaf_idx;
            }

            if common == prefix_len {
                let node_bit = bit_at(&node_prefix, prefix_len) as usize;
                let current_ts = self.slab.get(current).data.timestamp();
                let new_idx = self.alloc_announced_node(prefix, prefix_len, &incoming, ts);
                {
                    let n = self.slab.get_mut(new_idx);
                    n.data.merge_ancestor(current_ts);
                    n.children[node_bit] = Some(current);
                }
                self.set_parent_slot(parent, Some(new_idx), dirty);
                dirty(new_idx);
                dirty(current);
                self.propagate_up(&path[..path_len], ts, dirty);
                return new_idx;
            }

            // Split: `current` and the new leaf share `common` bits; branch below there.
            let split_prefix = mask_prefix(prefix, common);
            let leaf_bit = bit_at(&prefix, common) as usize;
            let node_bit = bit_at(&node_prefix, common) as usize;
            debug_assert_ne!(leaf_bit, node_bit);
            let current_ts = self.slab.get(current).data.timestamp();

            let leaf_idx = self.alloc_announced_node(prefix, prefix_len, &incoming, ts);
            let split_idx = self.alloc_split_node(split_prefix, common, current_ts);
            {
                let split = self.slab.get_mut(split_idx);
                split.children[leaf_bit] = Some(leaf_idx);
                split.children[node_bit] = Some(current);
            }
            self.set_parent_slot(parent, Some(split_idx), dirty);
            dirty(split_idx);
            dirty(leaf_idx);
            dirty(current);
            path[path_len] = Some(split_idx);
            path_len += 1;
            self.propagate_up(&path[..path_len], ts, dirty);
            return leaf_idx;
        }
    }

    pub fn withdraw(
        &mut self,
        prefix: [u32; 4],
        prefix_len: u32,
        ts: Timestamp,
        dirty: &mut impl FnMut(NodeIdx),
    ) -> bool {
        assert!(prefix_len <= MAX_PREFIX_LEN);
        let prefix = mask_prefix(prefix, prefix_len);

        let Some(mut current) = self.root else {
            return false;
        };
        let mut path: [Option<NodeIdx>; MAX_PREFIX_LEN as usize + 1] =
            [None; MAX_PREFIX_LEN as usize + 1];
        let mut path_len: usize = 0;

        loop {
            let (node_prefix, node_prefix_len, node_children, is_announced) = {
                let node = self.slab.get(current);
                (
                    node.prefix,
                    node.prefix_len,
                    node.children,
                    node.is_announced(),
                )
            };

            if prefix_len == node_prefix_len && prefix == node_prefix {
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

            let common = common_prefix_len(&prefix, prefix_len, &node_prefix, node_prefix_len);
            if common < node_prefix_len {
                return false;
            }
            let bit = bit_at(&prefix, node_prefix_len) as usize;
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

    fn alloc_announced_node(
        &mut self,
        prefix: [u32; 4],
        prefix_len: u32,
        incoming: &D,
        ts: Timestamp,
    ) -> NodeIdx {
        let idx = self.slab.alloc().expect("slab full");
        let mut data = D::default();
        data.apply_announce(incoming, ts);
        *self.slab.get_mut(idx) = Node {
            children: [None, None],
            prefix,
            prefix_len,
            flags: NodeFlags::ANNOUNCED,
            data,
        };
        idx
    }

    fn alloc_split_node(
        &mut self,
        prefix: [u32; 4],
        prefix_len: u32,
        subtree_ts: Timestamp,
    ) -> NodeIdx {
        let idx = self.slab.alloc().expect("slab full");
        let mut data = D::default();
        data.merge_ancestor(subtree_ts);
        *self.slab.get_mut(idx) = Node {
            children: [None, None],
            prefix,
            prefix_len,
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
            let common = common_prefix_len(&node.prefix, node.prefix_len, &addr, MAX_PREFIX_LEN);
            if common < node.prefix_len {
                return best;
            }
            if node.is_announced() {
                best = Some(current);
            }
            if node.prefix_len == MAX_PREFIX_LEN {
                return best;
            }
            let bit = bit_at(&addr, node.prefix_len) as usize;
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
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use super::*;
    use crate::slab::{SlabRead, VecSlab};
    use crate::thin::ThinData;
    use alloc::vec::Vec;

    type Tree = RadixTree<ThinData, VecSlab<Node<ThinData>>>;

    fn new_tree() -> Tree {
        RadixTree::new(VecSlab::new())
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> [u32; 4] {
        let word0 = ((a as u32) << 24) | ((b as u32) << 16) | ((c as u32) << 8) | (d as u32);
        [word0, 0, 0, 0]
    }

    fn ins(tree: &mut Tree, p: [u32; 4], len: u32, ts_ms: u64) -> NodeIdx {
        tree.insert(
            p,
            len,
            Timestamp::from_millis(ts_ms),
            ThinData::default(),
            &mut |_| {},
        )
    }

    fn wdw(tree: &mut Tree, p: [u32; 4], len: u32, ts_ms: u64) -> bool {
        tree.withdraw(p, len, Timestamp::from_millis(ts_ms), &mut |_| {})
    }

    #[test]
    fn empty_tree_lookup_returns_none() {
        let tree = new_tree();
        assert_eq!(tree.lookup(v4(10, 0, 0, 1)), None);
    }

    #[test]
    fn empty_tree_withdraw_returns_false() {
        let mut tree = new_tree();
        assert!(!wdw(&mut tree, v4(10, 0, 0, 0), 8, 1));
    }

    #[test]
    fn empty_tree_sweep_is_noop() {
        let mut tree = new_tree();
        tree.sweep_tombstones(&mut |_| {});
        assert!(tree.root().is_none());
    }

    #[test]
    fn insert_into_empty_tree_then_lookup() {
        let mut tree = new_tree();
        let idx = ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        assert_eq!(tree.lookup(v4(10, 1, 2, 3)), Some(idx));
        assert_eq!(tree.root(), Some(idx));
    }

    #[test]
    fn insert_two_disjoint_creates_split() {
        let mut tree = new_tree();
        let a = ins(&mut tree, v4(0, 0, 0, 0), 1, 100);
        let b = ins(&mut tree, v4(128, 0, 0, 0), 1, 200);
        let root = tree.root().unwrap();
        assert_ne!(root, a);
        assert_ne!(root, b);
        let root_node = tree.slab.get(root);
        assert_eq!(root_node.prefix_len, 0);
        assert!(!root_node.is_announced());
        assert_eq!(root_node.child_count(), 2);
        assert_eq!(tree.lookup(v4(1, 0, 0, 0)), Some(a));
        assert_eq!(tree.lookup(v4(200, 0, 0, 0)), Some(b));
    }

    #[test]
    fn insert_extend_then_ancestor_promotion() {
        let mut tree = new_tree();
        let long = ins(&mut tree, v4(10, 0, 0, 0), 16, 100);
        let short = ins(&mut tree, v4(10, 0, 0, 0), 8, 200);
        assert_eq!(tree.root(), Some(short));
        assert!(tree.slab.get(short).is_announced());
        assert!(tree.slab.get(long).is_announced());
        assert_eq!(tree.lookup(v4(10, 0, 0, 1)), Some(long));
        assert_eq!(tree.lookup(v4(10, 128, 0, 1)), Some(short));
    }

    #[test]
    fn insert_extend_existing_intermediate() {
        let mut tree = new_tree();
        let a = ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        let b = ins(&mut tree, v4(10, 128, 0, 0), 9, 200);
        let c = ins(&mut tree, v4(10, 0, 0, 0), 9, 300);
        assert_eq!(tree.lookup(v4(10, 200, 0, 0)), Some(b));
        assert_eq!(tree.lookup(v4(10, 1, 0, 0)), Some(c));
        assert_eq!(tree.lookup(v4(10, 0, 0, 0)), Some(c));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn lpm_finds_deepest_announced() {
        let mut tree = new_tree();
        let eight = ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        let sixteen = ins(&mut tree, v4(10, 1, 0, 0), 16, 100);
        assert_eq!(tree.lookup(v4(10, 1, 2, 3)), Some(sixteen));
        assert_eq!(tree.lookup(v4(10, 2, 2, 3)), Some(eight));
    }

    #[test]
    fn lookup_no_match_returns_none_in_populated_tree() {
        let mut tree = new_tree();
        ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        assert_eq!(tree.lookup(v4(192, 0, 2, 1)), None);
    }

    #[test]
    fn withdrawn_hidden_from_lookup_before_sweep() {
        let mut tree = new_tree();
        let a = ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        assert!(wdw(&mut tree, v4(10, 0, 0, 0), 8, 200));
        assert_eq!(tree.lookup(v4(10, 1, 2, 3)), None);
        assert!(!tree.slab.get(a).is_announced());
    }

    #[test]
    fn double_withdraw_returns_false() {
        let mut tree = new_tree();
        ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        assert!(wdw(&mut tree, v4(10, 0, 0, 0), 8, 200));
        assert!(!wdw(&mut tree, v4(10, 0, 0, 0), 8, 300));
    }

    #[test]
    fn timestamp_propagates_to_root() {
        let mut tree = new_tree();
        ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        ins(&mut tree, v4(128, 0, 0, 0), 8, 500);
        let root = tree.root().unwrap();
        assert_eq!(
            tree.slab.get(root).data.timestamp(),
            Timestamp::from_millis(500),
        );
    }

    #[test]
    fn timestamp_propagation_monotonic() {
        let mut tree = new_tree();
        ins(&mut tree, v4(10, 0, 0, 0), 8, 500);
        ins(&mut tree, v4(128, 0, 0, 0), 8, 200);
        let root = tree.root().unwrap();
        assert_eq!(
            tree.slab.get(root).data.timestamp(),
            Timestamp::from_millis(500),
        );
    }

    #[test]
    fn dirty_callback_emits_target_and_advanced_ancestors() {
        let mut tree = new_tree();
        ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        ins(&mut tree, v4(128, 0, 0, 0), 8, 100);
        let mut dirty: Vec<NodeIdx> = Vec::new();
        let leaf = tree.insert(
            v4(10, 1, 0, 0),
            16,
            Timestamp::from_millis(500),
            ThinData::default(),
            &mut |i| dirty.push(i),
        );
        assert!(dirty.contains(&leaf), "target must be dirty");
        let root = tree.root().unwrap();
        assert!(dirty.contains(&root), "root should be dirty (ts advanced)");
    }

    #[test]
    fn reannounce_reuses_slot() {
        let mut tree = new_tree();
        let a = ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        let len_before = tree.slab.len();
        assert!(wdw(&mut tree, v4(10, 0, 0, 0), 8, 200));
        let b = ins(&mut tree, v4(10, 0, 0, 0), 8, 300);
        assert_eq!(a, b, "re-announce should reuse the slot");
        assert_eq!(tree.slab.len(), len_before);
        assert!(tree.slab.get(a).is_announced());
    }

    #[test]
    fn sweep_collapses_withdrawn_chain() {
        let mut tree = new_tree();
        ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        ins(&mut tree, v4(10, 0, 0, 0), 16, 100);
        ins(&mut tree, v4(10, 0, 0, 0), 24, 100);
        assert!(wdw(&mut tree, v4(10, 0, 0, 0), 24, 200));
        assert!(wdw(&mut tree, v4(10, 0, 0, 0), 16, 200));
        assert!(wdw(&mut tree, v4(10, 0, 0, 0), 8, 200));
        let mut freed: Vec<NodeIdx> = Vec::new();
        tree.sweep_tombstones(&mut |i| freed.push(i));
        assert_eq!(tree.root(), None);
    }

    #[test]
    fn sweep_collapses_degenerate_split() {
        let mut tree = new_tree();
        ins(&mut tree, v4(0, 0, 0, 0), 8, 100);
        let b = ins(&mut tree, v4(128, 0, 0, 0), 8, 100);
        let root_before = tree.root().unwrap();
        assert!(!tree.slab.get(root_before).is_announced());
        assert!(wdw(&mut tree, v4(0, 0, 0, 0), 8, 200));
        tree.sweep_tombstones(&mut |_| {});
        assert_eq!(
            tree.root(),
            Some(b),
            "root should have collapsed to the remaining announced leaf",
        );
    }

    #[test]
    fn sweep_idempotent() {
        let mut tree = new_tree();
        ins(&mut tree, v4(10, 0, 0, 0), 8, 100);
        ins(&mut tree, v4(11, 0, 0, 0), 8, 100);
        assert!(wdw(&mut tree, v4(10, 0, 0, 0), 8, 200));
        tree.sweep_tombstones(&mut |_| {});
        let root_after_first = tree.root();
        let len_after_first = tree.slab.len();
        tree.sweep_tombstones(&mut |_| {});
        assert_eq!(tree.root(), root_after_first);
        assert_eq!(tree.slab.len(), len_after_first);
    }

    #[test]
    fn ipv6_lookup() {
        let mut tree = new_tree();
        let sixty_four = tree.insert(
            [0x2001_0db8, 0x1234_0000, 0, 0],
            64,
            Timestamp::from_millis(100),
            ThinData::default(),
            &mut |_| {},
        );
        let one_twenty_eight = tree.insert(
            [0x2001_0db8, 0x1234_0000, 0, 1],
            128,
            Timestamp::from_millis(100),
            ThinData::default(),
            &mut |_| {},
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

    fn naive_lpm(prefixes: &[([u32; 4], u32)], addr: [u32; 4]) -> Option<([u32; 4], u32)> {
        let mut best: Option<([u32; 4], u32)> = None;
        for &(p, len) in prefixes {
            let common = common_prefix_len(&p, len, &addr, MAX_PREFIX_LEN);
            if common == len {
                let better = match best {
                    None => true,
                    Some((_, best_len)) => len > best_len,
                };
                if better {
                    best = Some((p, len));
                }
            }
        }
        best
    }

    #[test]
    fn oracle_lpm_matches_naive_scan() {
        let mut tree = new_tree();
        let mut naive: Vec<([u32; 4], u32)> = Vec::new();
        let mut seed = 0xDEAD_BEEF_CAFE_BABEu64;

        for _ in 0..500 {
            let word0 = lcg(&mut seed);
            let prefix = [word0, 0, 0, 0];
            let prefix_len = 8 + (lcg(&mut seed) % 25);
            let masked = mask_prefix(prefix, prefix_len);
            ins(&mut tree, prefix, prefix_len, 100);
            naive.retain(|&(p, l)| !(p == masked && l == prefix_len));
            naive.push((masked, prefix_len));
        }

        for _ in 0..500 {
            let addr = [lcg(&mut seed), 0, 0, 0];
            let expected = naive_lpm(&naive, addr);
            let actual = tree.lookup(addr).map(|idx| {
                let n = tree.slab.get(idx);
                (n.prefix, n.prefix_len)
            });
            assert_eq!(actual, expected, "addr {:?}", addr);
        }
    }
}

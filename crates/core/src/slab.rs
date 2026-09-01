use crate::node::NodeIdx;

pub trait SlabRead<T> {
    fn get(&self, idx: NodeIdx) -> &T;
    /// Number of items currently allocated in the slab. Excludes both the
    /// reserved zero index and any slots that have been freed back to the pool.
    fn len(&self) -> u32;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub trait Slab<T>: SlabRead<T> {
    fn get_mut(&mut self, idx: NodeIdx) -> &mut T;
    fn alloc(&mut self) -> Option<NodeIdx>;
    fn free(&mut self, idx: NodeIdx);
    /// Allocated capacity for the slab in bytes ([size] is the size of the objects in the vec)
    fn size_capacity(&self) -> usize;
    /// Bytes actually being used by nodes in the vec
    fn size(&self) -> usize;
    /// Get if the index is valid/not freed
    fn try_get(&self, idx: NodeIdx) -> Option<&T>;
}

#[cfg(feature = "alloc")]
mod vec_backed {
    use super::*;
    use alloc::vec::Vec;
    use core::num::NonZeroU32;
    use hashbrown::HashSet;

    #[apply(Serde!)]
    #[derive(Clone)]
    pub struct VecSlab<T> {
        entries: Vec<T>,
        free_list: Vec<NodeIdx>,
        max_capacity: u32,
    }

    impl<T: Default> VecSlab<T> {
        pub fn new() -> Self {
            Self::with_capacity(u32::MAX)
        }

        pub fn with_capacity(cap: u32) -> Self {
            let initial_alloc = (cap.saturating_add(1)).min(1024) as usize;
            let mut entries = Vec::with_capacity(initial_alloc);
            entries.push(T::default());
            Self {
                entries,
                free_list: Vec::new(),
                max_capacity: cap,
            }
        }
    }

    impl<T> VecSlab<T> {
        /// Constructs a new VecSlab populated with the entries of [other], passed through a mapping
        /// function [mapper]
        ///
        /// Note: currently all entries (including ones in the free list) in [other] are mapped. A
        /// future optimization could be to skip those, but I suspect that this big sequential memory
        /// copy is actually quite fast in practice so it's prob not necessary
        ///
        /// # Examples
        /// ```
        /// use slash0_core::slab::{VecSlab, Slab, SlabRead};
        /// let mut original = VecSlab::new();
        /// let a = original.alloc().unwrap();
        /// *original.get_mut(a) = "foo".to_string();
        ///
        /// let new = VecSlab::with_entries_mapped_from(&original, |i| i.to_uppercase());
        /// assert_eq!(new.get(a), "FOO");
        /// ```
        pub fn with_entries_mapped_from<U, F>(other: &VecSlab<U>, mapper: F) -> Self
        where
            F: FnMut(&U) -> T,
        {
            let entries = other.entries.iter().map(mapper).collect();
            Self {
                entries,
                free_list: other.free_list.clone(),
                max_capacity: other.max_capacity,
            }
        }

        /// The backing entries as one contiguous slice, index-aligned: element
        /// `i` is the entry at [`NodeIdx`] `i`. Includes the reserved slot 0 and
        /// any freed holes, so a consumer can copy the whole thing (e.g. a GPU
        /// bulk upload) without translating indices.
        pub fn as_slice(&self) -> &[T] {
            &self.entries
        }
    }

    impl<T: Default> Default for VecSlab<T> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<T> SlabRead<T> for VecSlab<T> {
        fn get(&self, idx: NodeIdx) -> &T {
            &self.entries[idx.get() as usize]
        }

        fn len(&self) -> u32 {
            // `entries` carries the reserved zero slot and never shrinks, so the
            // live count is the backing length minus that slot and everything
            // currently parked on the free list.
            (self.entries.len() - 1 - self.free_list.len()) as u32
        }
    }

    impl<T: Default> Slab<T> for VecSlab<T> {
        fn get_mut(&mut self, idx: NodeIdx) -> &mut T {
            &mut self.entries[idx.get() as usize]
        }

        fn alloc(&mut self) -> Option<NodeIdx> {
            if let Some(idx) = self.free_list.pop() {
                return Some(idx);
            }
            let next_position = self.entries.len();
            if next_position > self.max_capacity as usize {
                return None;
            }
            self.entries.push(T::default());
            NonZeroU32::new(next_position as u32)
        }

        fn free(&mut self, idx: NodeIdx) {
            debug_assert!(
                !self.free_list.contains(&idx),
                "double-free of slab index {}",
                idx.get()
            );
            self.free_list.push(idx);
        }
        fn size_capacity(&self) -> usize {
            size_of::<T>() * self.entries.capacity()
        }
        fn size(&self) -> usize {
            size_of::<T>() * self.entries.len()
        }

        fn try_get(&self, idx: NodeIdx) -> Option<&T> {
            // Linear search is eww, but the free list shouldn't be that big typically
            // and I think fast push/pop operations are much more important so I don't wanna make
            // it a HashSet anyway. Can revisit later
            if self.free_list.contains(&idx) || idx.get() as usize > self.size() {
                return None;
            };

            self.entries.get(idx.get() as usize)
        }
    }

    impl<'a, T> IntoIterator for &'a VecSlab<T> {
        type Item = &'a T;
        type IntoIter = VecSlabIterator<'a, T>;

        fn into_iter(self) -> Self::IntoIter {
            VecSlabIterator::new(self)
        }
    }

    pub struct VecSlabIterator<'a, T> {
        entries: &'a Vec<T>,
        free_set: HashSet<usize>,
        cur_idx: usize,
    }

    impl<'a, T> VecSlabIterator<'a, T> {
        fn new(slab: &'a VecSlab<T>) -> Self {
            Self {
                entries: &slab.entries,
                free_set: HashSet::from_iter(slab.free_list.iter().map(|i| (*i).get() as usize)),
                cur_idx: 0,
            }
        }
    }

    impl<'a, T> Iterator for VecSlabIterator<'a, T> {
        type Item = &'a T;

        fn next(&mut self) -> Option<Self::Item> {
            self.cur_idx += 1;
            while self.free_set.contains(&self.cur_idx) && self.cur_idx < self.entries.len() {
                self.cur_idx += 1;
            }

            self.entries.get(self.cur_idx)
        }
    }
}

#[cfg(feature = "alloc")]
pub use vec_backed::VecSlab;

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use super::*;
    use core::num::NonZeroU32;

    fn idx(n: u32) -> NodeIdx {
        NonZeroU32::new(n).unwrap()
    }

    #[test]
    fn alloc_returns_sequential_indices_starting_at_one() {
        let mut slab = VecSlab::<u32>::new();
        assert!(slab.is_empty());
        assert_eq!(slab.alloc(), Some(idx(1)));
        assert_eq!(slab.alloc(), Some(idx(2)));
        assert_eq!(slab.alloc(), Some(idx(3)));
        assert!(!slab.is_empty());
    }

    #[test]
    fn free_reuses_slot_lifo() {
        let mut slab = VecSlab::<u32>::new();
        let a = slab.alloc().unwrap();
        let b = slab.alloc().unwrap();
        let c = slab.alloc().unwrap();
        slab.free(b);
        assert_eq!(slab.alloc(), Some(b));
        slab.free(c);
        slab.free(a);
        assert_eq!(slab.alloc(), Some(a));
        assert_eq!(slab.alloc(), Some(c));
    }

    #[test]
    fn reuse_before_grow() {
        let mut slab = VecSlab::<u32>::new();
        let a = slab.alloc().unwrap();
        let _b = slab.alloc().unwrap();
        let len_before = slab.len();
        slab.free(a);
        let reused = slab.alloc().unwrap();
        assert_eq!(reused, a);
        assert_eq!(slab.len(), len_before);
    }

    #[test]
    fn growing_preserves_state_across_reallocation() {
        let mut slab = VecSlab::<u32>::with_capacity(u32::MAX);
        let mut handles = alloc::vec::Vec::new();
        for i in 0..1000u32 {
            let idx = slab.alloc().unwrap();
            *slab.get_mut(idx) = i * 7 + 3;
            handles.push((idx, i * 7 + 3));
        }
        for (idx, expected) in handles {
            assert_eq!(*slab.get(idx), expected);
        }
    }

    #[test]
    fn clone_is_an_independent_deep_copy() {
        let mut original = VecSlab::<u32>::new();
        for _ in 0..16 {
            let idx = original.alloc().unwrap();
            *original.get_mut(idx) = idx.get();
        }

        let mut clone = original.clone();

        // Zeroing every entry in the clone must not disturb the original. No
        // slots were freed, so the live indices are 1..=len contiguously.
        for i in 1..=clone.len() {
            *clone.get_mut(idx(i)) = 0;
        }

        for i in 1..=original.len() {
            assert_eq!(*original.get(idx(i)), i);
        }
    }

    #[test]
    fn len_counts_live_allocations_not_freed_slots() {
        let mut slab = VecSlab::<u32>::new();
        assert_eq!(slab.len(), 0);
        assert!(slab.is_empty());

        let a = slab.alloc().unwrap();
        let b = slab.alloc().unwrap();
        let _c = slab.alloc().unwrap();
        assert_eq!(slab.len(), 3);

        slab.free(a);
        slab.free(b);
        assert_eq!(slab.len(), 1, "freed slots drop out of the count");

        slab.alloc().unwrap();
        assert_eq!(slab.len(), 2, "reallocating a freed slot counts again");
    }

    #[test]
    fn oom_returns_none_at_capacity() {
        let mut slab = VecSlab::<u32>::with_capacity(3);
        let a = slab.alloc().unwrap();
        let _b = slab.alloc().unwrap();
        let _c = slab.alloc().unwrap();
        assert_eq!(a, idx(1));
        assert_eq!(slab.alloc(), None);
        slab.free(a);
        assert_eq!(slab.alloc(), Some(a));
        assert_eq!(slab.alloc(), None);
    }

    #[test]
    fn test_iter_over_slab() {
        let mut slab = VecSlab::<alloc::string::String>::new();
        let a = slab.alloc().unwrap();
        let b = slab.alloc().unwrap();
        let c = slab.alloc().unwrap();

        *slab.get_mut(a) = "a".into();
        *slab.get_mut(b) = "b".into();
        *slab.get_mut(c) = "c".into();

        slab.free(b);

        let mut collector = alloc::vec![];
        for i in &slab {
            collector.push(i);
        }

        assert_eq!(collector, alloc::vec!["a", "c"]);
    }
}

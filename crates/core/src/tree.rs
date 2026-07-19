use crate::node::NodeData;
use crate::slab::Slab;
use core::marker::PhantomData;

pub struct RadixTree<D: NodeData, S: Slab<D>> {
    pub slab: S,
    _phantom: PhantomData<D>,
}

impl<D: NodeData, S: Slab<D>> RadixTree<D, S> {
    pub fn new(slab: S) -> Self {
        Self {
            slab,
            _phantom: PhantomData,
        }
    }
}

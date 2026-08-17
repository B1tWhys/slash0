use slash0_core::node::{Node, NodeIdx};
use slash0_core::slab::SlabRead;
use slash0_core::thin::ThinData;

#[derive(Copy, Clone)]
pub struct GpuSlab<'a> {
    elements: &'a [Node<ThinData>],
}

impl<'a> GpuSlab<'a> {
    pub fn new(tree_slab: &'a [Node<ThinData>]) -> Self {
        Self {
            elements: tree_slab,
        }
    }
}

impl<'a> SlabRead<Node<ThinData>> for GpuSlab<'a> {
    fn get(&self, idx: NodeIdx) -> &Node<ThinData> {
        &self.elements[idx.get() as usize]
    }

    // TODO: Remove this from the SlabRead trait (and corresponding RadixTree<_, SlabRead> functions). It's not
    // necessary for GPU and not possible to implement easily
    fn len(&self) -> u32 {
        self.elements.len() as u32
    }
}

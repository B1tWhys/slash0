use crate::node::NodeData;

pub trait Slab<D: NodeData> {
    fn get(&self, idx: u32) -> &D;
    fn get_mut(&mut self, idx: u32) -> &mut D;
    fn len(&self) -> u32;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(feature = "alloc")]
mod vec_backed {
    use super::*;
    use alloc::vec::Vec;

    pub struct VecSlab<D>(pub Vec<D>);

    impl<D: NodeData> Slab<D> for VecSlab<D> {
        fn get(&self, idx: u32) -> &D {
            &self.0[idx as usize]
        }
        fn get_mut(&mut self, idx: u32) -> &mut D {
            &mut self.0[idx as usize]
        }
        fn len(&self) -> u32 {
            self.0.len() as u32
        }
    }
}

#[cfg(feature = "alloc")]
pub use vec_backed::VecSlab;

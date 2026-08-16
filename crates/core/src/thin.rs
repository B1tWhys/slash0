#[cfg(feature = "gpu")]
use crate::node::Node;
use crate::node::{NodeData, ThinNodeData};
use crate::timestamp::Timestamp;

#[apply(Serde!)]
#[apply(Pod!)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ThinData {
    pub timestamp: Timestamp,
}

impl NodeData for ThinData {
    fn timestamp(&self) -> Timestamp {
        self.timestamp
    }
    fn set_timestamp(&mut self, ts: Timestamp) {
        self.timestamp = ts;
    }
    fn apply_announce(&mut self, _incoming: &Self, ts: Timestamp) {
        self.timestamp = ts;
    }
    fn apply_withdraw(&mut self, ts: Timestamp) {
        self.timestamp = ts;
    }
}

impl ThinNodeData for ThinData {}

// bytemuck's derive can't prove a generic Node<D> is padding-free, so Pod/Zeroable
// are impl'd for the one monomorphization the client actually uploads. Sound because
// Node<ThinData> is a #[repr(C)] struct of only 4-byte-aligned fields whose every bit
// pattern is valid (the Option<NonZeroU32> children map 0 -> None), and it has no
// padding -- see thin_node_has_no_padding in node.rs.
#[cfg(feature = "gpu")]
unsafe impl bytemuck::Zeroable for Node<ThinData> {}
#[cfg(feature = "gpu")]
unsafe impl bytemuck::Pod for Node<ThinData> {}

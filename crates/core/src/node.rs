use crate::prefix::Prefix;
use crate::timestamp::Timestamp;
use core::num::NonZeroU32;

pub type NodeIdx = NonZeroU32;

bitflags::bitflags! {
    #[apply(Serde!)]
    #[repr(transparent)]
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct NodeFlags: u32 {
        const ANNOUNCED = 1 << 0;
    }
}

pub trait NodeData: Default {
    fn timestamp(&self) -> Timestamp;
    fn set_timestamp(&mut self, ts: Timestamp);

    /// Applied to the target node when an announce arrives. Must overwrite
    /// this payload with `incoming`'s payload and set the timestamp to `ts`.
    fn apply_announce(&mut self, incoming: &Self, ts: Timestamp);

    /// Applied to the target node when a withdraw arrives. Must clear payload
    /// fields to their defaults and set the timestamp to `ts`.
    fn apply_withdraw(&mut self, ts: Timestamp);

    /// Applied to each ancestor on the path from target to root during
    /// max-to-root propagation. Override if future aggregates need to
    /// propagate alongside the timestamp.
    fn merge_ancestor(&mut self, descendant_ts: Timestamp) {
        if descendant_ts > self.timestamp() {
            self.set_timestamp(descendant_ts);
        }
    }
}

/// Marker for node data suitable for GPU buffer upload and shader use:
/// bitwise-copyable, no owned heap allocations, no drop glue. `ThinData`
/// implements this; `ThickData` does not.
pub trait ThinNodeData: NodeData + Copy {}

#[apply(Serde!)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Node<D> {
    pub children: [Option<NodeIdx>; 2],
    pub prefix: Prefix,
    pub flags: NodeFlags,
    pub data: D,
}

impl<D> Node<D> {
    pub fn is_announced(&self) -> bool {
        self.flags.contains(NodeFlags::ANNOUNCED)
    }

    pub fn set_announced(&mut self, on: bool) {
        self.flags.set(NodeFlags::ANNOUNCED, on);
    }

    pub fn child_count(&self) -> u32 {
        self.children[0].is_some() as u32 + self.children[1].is_some() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::{Node, NodeFlags, NodeIdx};
    use crate::prefix::Prefix;
    use crate::thin::ThinData;
    use core::mem::size_of;

    #[test]
    fn thin_node_has_no_padding() {
        // The CPU uploads Node<ThinData> in its #[repr(C)] layout and the shader
        // reads it back in std430. Those agree only while every field is a
        // 4-byte-aligned u32/[u32; N] and the struct is densely packed. This
        // survives adding more u32 fields (size just grows on both sides) but
        // fires the moment a field introduces padding -- the signal to go
        // re-check that the two layouts still line up.
        let fields = size_of::<[Option<NodeIdx>; 2]>()
            + size_of::<Prefix>()
            + size_of::<NodeFlags>()
            + size_of::<ThinData>();
        assert_eq!(size_of::<Node<ThinData>>(), fields);
    }
}

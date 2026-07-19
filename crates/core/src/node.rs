use crate::timestamp::Timestamp;

pub trait NodeData: Default {
    fn timestamp(&self) -> Timestamp;
    fn set_timestamp(&mut self, ts: Timestamp);
}

/// Marker for node data suitable for GPU buffer upload and shader use:
/// bitwise-copyable, no owned heap allocations, no drop glue. `ThinData`
/// implements this; `ThickData` does not.
pub trait ThinNodeData: NodeData + Copy {}

use crate::node::{NodeData, ThinNodeData};
use crate::timestamp::Timestamp;

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

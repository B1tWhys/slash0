use crate::node::NodeData;
use crate::thin::ThinData;
use crate::timestamp::Timestamp;

#[derive(Clone, Debug, Default)]
pub struct ThickData {
    pub timestamp: Timestamp,
}

impl NodeData for ThickData {
    fn timestamp(&self) -> Timestamp {
        self.timestamp
    }
    fn set_timestamp(&mut self, ts: Timestamp) {
        self.timestamp = ts;
    }
    fn apply_announce(&mut self, incoming: &Self, ts: Timestamp) {
        *self = incoming.clone();
        self.timestamp = ts;
    }
    fn apply_withdraw(&mut self, ts: Timestamp) {
        *self = Self::default();
        self.timestamp = ts;
    }
}

impl From<&ThickData> for ThinData {
    fn from(t: &ThickData) -> Self {
        Self {
            timestamp: t.timestamp,
        }
    }
}

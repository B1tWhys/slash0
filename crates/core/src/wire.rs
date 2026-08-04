use crate::node::Node;
use crate::prefix::{IpVersion, Prefix};
use crate::slab::VecSlab;
use crate::thin::ThinData;
use crate::timestamp::Timestamp;
use crate::tree::RadixTree;

#[apply(Serde!)]
#[derive(Debug)]
pub enum Slash0Message {
    SubscribeRequest {
        ip_version: IpVersion,
    },
    TrieSnapshot {
        ip_version: IpVersion,
        tree: RadixTree<ThinData, VecSlab<Node<ThinData>>>, // TODO: last_bgp_update_id
    },
    ThinBgpUpdate(ThinBgpUpdate),
}

#[apply(Serde!)]
#[derive(Debug)]
pub struct ThinBgpUpdate {
    prefix: Prefix,
    timestamp: Timestamp,
}

impl ThinBgpUpdate {
    pub fn new(prefix: Prefix, timestamp: Timestamp) -> Self {
        Self { prefix, timestamp }
    }
}

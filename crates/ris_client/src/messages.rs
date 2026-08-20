//! Rust type definitions for the RIS Live protocol.
//! https://ris-live.ripe.net/manual/

use serde::{Deserialize, Serialize};
use slash0_core::prefix::Prefix;
use slash0_core::timestamp::Timestamp;
use slash0_core::wire::{ThinBgpUpdate, UpdateType};
use std::collections::HashMap;

/// Textual IP address as delivered by RIS Live. Left as a string because some
/// fields (e.g. a next hop) carry a comma-joined pair of addresses rather than
/// a single parseable address.
type IpAddr = String;

// ============================================================================
// Top-level envelope
// ============================================================================

/// Messages the client sends to the server.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum ClientMessage {
    RisSubscribe(RisSubscribe),
    RisUnsubscribe(SubscriptionFilters),
    RequestRrcList,
    Ping,
}

/// Messages the server sends to the client.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerMessage {
    RisMessage(RisMessage),
    RisError(RisError),
    RisRrcList(Vec<String>),
    RisSubscribeOk(RisSubscribeOk),
    Pong,
}

// ============================================================================
// Client message payloads
// ============================================================================

/// Payload of a `ris_subscribe` message: a set of filters plus socket options.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct RisSubscribe {
    #[serde(flatten)]
    pub filters: SubscriptionFilters,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket_options: Option<SocketOptions>,
}

/// The filter portion of a subscription (also used for `ris_unsubscribe` and
/// echoed back inside `ris_subscribe_ok`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionFilters {
    /// Only include messages collected by a particular RRC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Only include messages of a given BGP or RIS type.
    /// `type` is a reserved word, so this field needs an explicit rename.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub message_type: Option<BgpMessageType>,
    /// Only include messages containing a given key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require: Option<Require>,
    /// Only include messages sent by the given BGP peer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// ASN or pattern to match against the AS_PATH attribute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathFilter>,
    /// Filter UPDATE messages by prefixes in announcements or withdrawals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<PrefixFilter>,
    /// Match prefixes more specific than `prefix` (default: true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more_specific: Option<bool>,
    /// Match prefixes less specific than `prefix` (default: false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub less_specific: Option<bool>,
}

/// `require` filter value. Documented values are the two below; switch to a
/// `String` if you need to match arbitrary keys.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Require {
    Announcements,
    Withdrawals,
}

/// `path` filter: either a single ASN or an AS-path pattern string.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum PathFilter {
    Asn(u32),
    Pattern(String),
}

/// `prefix` filter: a single CIDR prefix or an array of them.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum PrefixFilter {
    Single(String),
    Multiple(Vec<String>),
}

/// Options applying to all subscriptions over the current WebSocket.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct SocketOptions {
    /// Include a Base64-encoded copy of the original binary BGP message as
    /// `raw` (default: false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_raw: Option<bool>,
    /// Send a `ris_subscribe_ok` for successful subscriptions (default: false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledge: Option<bool>,
}

// ============================================================================
// Server message payloads
// ============================================================================

/// Payload of a `ris_error` message.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RisError {
    pub message: String,
}

/// Payload of a `ris_subscribe_ok` message.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RisSubscribeOk {
    /// The user-supplied parameters of the subscription (minus socketOptions).
    pub subscription: SubscriptionFilters,
    /// The (possibly modified) set of currently applied socket options.
    pub socket_options: SocketOptions,
}

// ============================================================================
// ris_message
// ============================================================================

/// A BGP message relayed from a peered router, or a route-collector metadata
/// update. Common fields are shared; type-specific fields live in `body`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RisMessage {
    /// Time the message was received by the RRC (UNIX seconds, fractional).
    pub timestamp: f32,
    /// IP address of the BGP peer that sent this message.
    pub peer: IpAddr,
    /// Autonomous system of the peer (kept as a string, as in the protocol).
    pub peer_asn: String,
    /// Globally unique, per-session sequentially sortable message identifier.
    pub id: String,
    /// Hostname of the RRC that received this message.
    pub host: String,
    /// Hex encoding of the raw BGP message; present only if `includeRaw` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Type-specific contents. The `type` discriminator is shared with this.
    #[serde(flatten)]
    pub body: RisMessageBody,
}

/// Type-dependent fields of a `ris_message`, keyed on the `type` field.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RisMessageBody {
    Update(BgpUpdate),
    Keepalive,
    Open(BgpOpen),
    Notification(BgpNotification),
    RisPeerState(RisPeerState),
}

/// BGP UPDATE payload.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BgpUpdate {
    /// AS_PATH attribute: ASNs starting with the first upstream, possibly
    /// ending in an AS_SET.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Vec<PathSegment>>,
    /// COMMUNITY attribute: (ASN, value) pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community: Option<Vec<Community>>,
    /// ORIGIN attribute, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// MULTI_EXIT_DISC attribute, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub med: Option<u32>,
    /// AGGREGATOR attribute, if present (e.g. "64496:192.0.2.0").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator: Option<String>,
    /// Announced prefixes grouped by next hop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announcements: Option<Vec<Announcement>>,
    /// Withdrawn CIDR prefixes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withdrawals: Option<Vec<String>>,
}

impl BgpUpdate {
    pub fn to_thin_updates(
        &self,
        ts: Timestamp,
        ip_version: slash0_core::prefix::IpVersion,
    ) -> Vec<ThinBgpUpdate> {
        let announced_prefixes = self
            .announcements
            .iter()
            .flatten()
            .flat_map(|announcement| &announcement.prefixes)
            .map(|prefix| (prefix, UpdateType::ANNOUNCE));

        let withdrawn_prefixes = self
            .withdrawals
            .iter()
            .flatten()
            .map(|prefix| (prefix, UpdateType::WITHDRAW));

        announced_prefixes
            .chain(withdrawn_prefixes)
            .flat_map(|(prefix, update_type)| {
                Prefix::parse_cidr(prefix).map(|prefix| (prefix, update_type))
            })
            .filter(|((version, _), _)| version == &ip_version)
            .map(|((_, prefix), update_type)| ThinBgpUpdate::new(prefix, ts, update_type))
            .collect()
    }
}

/// One element of an AS_PATH: a single ASN, or an AS_SET (ascending order).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum PathSegment {
    Asn(u32),
    AsSet(Vec<u32>),
}

/// A COMMUNITY attribute pairing an ASN with a community value. Serializes as
/// the two-element array `[asn, value]`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Community(pub u32, pub u32);

/// ORIGIN path attribute.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Origin {
    Igp,
    Egp,
    Incomplete,
}

/// A set of prefixes announced via a common next hop.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Announcement {
    pub next_hop: IpAddr,
    pub prefixes: Vec<String>,
}

/// BGP OPEN payload.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BgpOpen {
    pub direction: Direction,
    pub version: u8,
    pub asn: u32,
    pub hold_time: u16,
    pub router_id: String,
    /// Capability code (as a string key) -> capability description. The shape
    /// varies per capability, so values are left as raw JSON.
    pub capabilities: HashMap<String, serde_json::Value>,
}

/// Direction of an OPEN/NOTIFICATION relative to the route collector.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Sent,
    Received,
}

/// BGP NOTIFICATION payload.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BgpNotification {
    pub notification: Notification,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Notification {
    pub code: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subcode: Option<u8>,
    /// Hex-encoded notification data, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// RIS peer state-change payload.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RisPeerState {
    pub state: PeerState,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PeerState {
    Connected,
    Up,
    Down,
}

/// BGP/RIS message type, used as the `type` filter in a subscription.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BgpMessageType {
    Update,
    Open,
    Notification,
    Keepalive,
    RisPeerState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ris_update_message() {
        let s = include_bytes!("test_ris_message.json");
        let message: ServerMessage = serde_json::from_slice(s).unwrap();
        let ServerMessage::RisMessage(ris) = message else {
            panic!("expected a ris_message envelope");
        };
        assert!(matches!(ris.body, RisMessageBody::Update(_)));
    }
}

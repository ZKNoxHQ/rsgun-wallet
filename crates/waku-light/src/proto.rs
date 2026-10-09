//! Waku RPC messages, hand-derived with prost (no build script, no protoc).
//!
//! Field numbers and types follow waku-org/waku-proto: `waku/message/v1`, `waku/filter/v2`,
//! `waku/lightpush/v2` and `v3`, `waku/metadata/v1`.

/// `waku.message.v1.WakuMessage`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct WakuMessage {
    #[prost(bytes = "vec", tag = "1")]
    pub payload: Vec<u8>,
    #[prost(string, tag = "2")]
    pub content_topic: String,
    #[prost(uint32, optional, tag = "3")]
    pub version: Option<u32>,
    /// Nanoseconds since the epoch; zigzag encoded on the wire.
    #[prost(sint64, optional, tag = "10")]
    pub timestamp: Option<i64>,
    #[prost(bytes = "vec", optional, tag = "11")]
    pub meta: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "21")]
    pub rate_limit_proof: Option<Vec<u8>>,
    #[prost(bool, optional, tag = "31")]
    pub ephemeral: Option<bool>,
}

// ---- filter v2 ----

pub mod filter_subscribe_type {
    pub const SUBSCRIBER_PING: i32 = 0;
    pub const SUBSCRIBE: i32 = 1;
    pub const UNSUBSCRIBE: i32 = 2;
    pub const UNSUBSCRIBE_ALL: i32 = 3;
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FilterSubscribeRequest {
    #[prost(string, tag = "1")]
    pub request_id: String,
    /// See [`filter_subscribe_type`].
    #[prost(int32, tag = "2")]
    pub filter_subscribe_type: i32,
    #[prost(string, optional, tag = "10")]
    pub pubsub_topic: Option<String>,
    #[prost(string, repeated, tag = "11")]
    pub content_topics: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FilterSubscribeResponse {
    #[prost(string, tag = "1")]
    pub request_id: String,
    #[prost(uint32, tag = "10")]
    pub status_code: u32,
    #[prost(string, optional, tag = "11")]
    pub status_desc: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MessagePush {
    #[prost(message, optional, tag = "1")]
    pub waku_message: Option<WakuMessage>,
    #[prost(string, optional, tag = "2")]
    pub pubsub_topic: Option<String>,
}

// ---- light push v2 (2.0.0-beta1) ----

#[derive(Clone, PartialEq, prost::Message)]
pub struct PushRequest {
    #[prost(string, tag = "1")]
    pub pubsub_topic: String,
    #[prost(message, optional, tag = "2")]
    pub message: Option<WakuMessage>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PushResponse {
    #[prost(bool, tag = "1")]
    pub is_success: bool,
    #[prost(string, optional, tag = "2")]
    pub info: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PushRpc {
    #[prost(string, tag = "1")]
    pub request_id: String,
    #[prost(message, optional, tag = "2")]
    pub request: Option<PushRequest>,
    #[prost(message, optional, tag = "3")]
    pub response: Option<PushResponse>,
}

// ---- light push v3 (3.0.0) ----

#[derive(Clone, PartialEq, prost::Message)]
pub struct LightpushRequest {
    #[prost(string, tag = "1")]
    pub request_id: String,
    #[prost(string, optional, tag = "20")]
    pub pubsub_topic: Option<String>,
    #[prost(message, optional, tag = "21")]
    pub message: Option<WakuMessage>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LightpushResponse {
    #[prost(string, tag = "1")]
    pub request_id: String,
    #[prost(uint32, tag = "10")]
    pub status_code: u32,
    #[prost(string, optional, tag = "11")]
    pub status_desc: Option<String>,
    #[prost(uint32, optional, tag = "12")]
    pub relay_peer_count: Option<u32>,
}

// ---- metadata v1 ----

/// Shards are written unpacked, as nwaku does; prost decodes both forms.
#[derive(Clone, PartialEq, prost::Message)]
pub struct WakuMetadataRequest {
    #[prost(uint32, optional, tag = "1")]
    pub cluster_id: Option<u32>,
    #[prost(uint32, repeated, packed = "false", tag = "2")]
    pub shards: Vec<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WakuMetadataResponse {
    #[prost(uint32, optional, tag = "1")]
    pub cluster_id: Option<u32>,
    #[prost(uint32, repeated, packed = "false", tag = "2")]
    pub shards: Vec<u32>,
}

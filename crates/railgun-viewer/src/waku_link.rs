//! Link with the js-waku light node of the page (the wallet's bundle), shared by the daemon
//! (`/api/defaults`, `/api/waku/exchange` over HTTP) and the web build (same routes, in-page).

use base64::{Engine as _, engine::general_purpose::STANDARD};
use railgun_broadcaster::{BrowserBridge, PublishAck, RemoteStatus, transport::WakuMessage};
use serde_json::{Value, json};

/// Same shape as the wallet's `/api/defaults`: the part the page's Waku node needs, and whether
/// this build has its own Rust light node (`native_waku`: the daemon yes, the web build not yet).
pub fn defaults(native_waku: bool) -> Value {
    json!({
        "nativeWaku": native_waku,
        "waku": {
            "clusterId": railgun_broadcaster::wire::CLUSTER_ID,
            "shardId": railgun_broadcaster::wire::SHARD_ID,
            "bootstrapPeers": railgun_broadcaster::wire::FLEET_WSS_PEERS,
        }
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeMessage {
    content_topic: String,
    payload: String,
    #[serde(default)]
    timestamp_ns: Option<String>,
}

#[derive(serde::Deserialize)]
struct ExchangeAck {
    id: u64,
    #[serde(default)]
    peers: usize,
    #[serde(default)]
    error: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeBody {
    connected: bool,
    #[serde(default)]
    peers: usize,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    messages: Vec<ExchangeMessage>,
    #[serde(default)]
    acks: Vec<ExchangeAck>,
}

/// One round trip of the page's Waku node: received messages in, publishes out.
pub fn exchange(bridge: &BrowserBridge, body: ExchangeBody) -> Value {
    let received: Vec<WakuMessage> = body
        .messages
        .into_iter()
        .filter_map(|m| {
            Some(WakuMessage {
                content_topic: m.content_topic,
                payload: STANDARD.decode(m.payload.as_bytes()).ok()?,
                timestamp_ns: m.timestamp_ns.and_then(|t| t.parse().ok()),
            })
        })
        .collect();
    let acks = body
        .acks
        .into_iter()
        .map(|a| PublishAck { id: a.id, peers: a.peers, error: a.error })
        .collect();
    let publish: Vec<Value> = bridge
        .exchange(
            RemoteStatus { connected: body.connected, peers: body.peers, detail: body.detail },
            received,
            acks,
        )
        .into_iter()
        .map(|o| json!({ "id": o.id, "contentTopic": o.content_topic, "payload": STANDARD.encode(&o.payload) }))
        .collect();
    json!({ "publish": publish })
}

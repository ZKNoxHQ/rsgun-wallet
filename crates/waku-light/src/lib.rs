//! Native Waku light client: what a js-waku light node does, on rust-libp2p.
//!
//! * transport: WebSocket (`/ws` and `/wss`, rustls with webpki roots) over DNS + TCP, noise,
//!   yamux or mplex; this is what the Railgun fleet exposes on `:8000`;
//! * `/vac/waku/filter-subscribe/2.0.0-beta1` and `/vac/waku/filter-push/2.0.0-beta1` to
//!   receive, with a subscriber ping that keeps the subscription alive on the service node;
//! * `/vac/waku/lightpush/3.0.0`, or `2.0.0-beta1` when the peer does not announce v3, to publish;
//! * `/vac/waku/metadata/1.0.0` both ways: nwaku drops a peer that does not answer it on a
//!   sharded cluster;
//! * identify and ping.
//!
//! No relay, no store, no discovery: only the configured bootstrap peers are dialled, and redialled
//! when they drop. Received messages are deduplicated by their deterministic Waku hash, since
//! every service node pushes the same message.

mod codec;
mod filter;
mod hash;
mod lightpush;
mod metadata;
mod node;
pub mod proto;
mod rt;

use std::time::Duration;

pub use libp2p::{Multiaddr, PeerId};
pub use node::LightNode;
use thiserror::Error;

pub mod protocols {
    pub const FILTER_SUBSCRIBE: &str = "/vac/waku/filter-subscribe/2.0.0-beta1";
    pub const FILTER_PUSH: &str = "/vac/waku/filter-push/2.0.0-beta1";
    pub const LIGHTPUSH_V2: &str = "/vac/waku/lightpush/2.0.0-beta1";
    pub const LIGHTPUSH_V3: &str = "/vac/waku/lightpush/3.0.0";
    pub const METADATA: &str = "/vac/waku/metadata/1.0.0";
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Multiaddrs, `/p2p/<peer id>` included. It may be left out on `/wss` addresses, where the
    /// certificate authenticates the host; it is required otherwise.
    pub bootstrap: Vec<Multiaddr>,
    /// When a pinned `/wss` bootstrap peer answers with another identity (key rotated on the
    /// fleet side), drop the pin and keep the connection: TLS already authenticated the host.
    /// Pins on non-wss addresses are always enforced.
    pub accept_rotated_wss_identity: bool,
    pub cluster_id: u32,
    /// Shards announced in the metadata handshake.
    pub shards: Vec<u32>,
    /// Static-sharding pubsub topic every filter and light push request carries.
    pub pubsub_topic: String,
    /// Service nodes to hold a filter subscription with (all connected ones up to this).
    pub max_filter_peers: usize,
    /// Service nodes a publish goes to.
    pub max_push_peers: usize,
    /// Subscriber ping period. nwaku expires a silent subscription after 5 minutes.
    pub filter_ping_interval: Duration,
    /// Deadline of one request/response exchange.
    pub request_timeout: Duration,
    /// Deadline of a dial: DNS, TCP, TLS, WebSocket upgrade, noise and muxer. relay-a of the
    /// Railgun fleet takes about 10 s and sometimes more than 20.
    pub dial_timeout: Duration,
    /// First redial delay of a lost bootstrap peer, doubled up to `max_redial_delay`.
    pub redial_delay: Duration,
    pub max_redial_delay: Duration,
    /// Received messages kept until drained; the oldest go first.
    pub inbox_capacity: usize,
    pub agent_version: String,
}

impl Config {
    pub fn new(bootstrap: Vec<Multiaddr>, cluster_id: u32, shard: u32) -> Self {
        Self {
            bootstrap,
            accept_rotated_wss_identity: true,
            cluster_id,
            shards: vec![shard],
            pubsub_topic: format!("/waku/2/rs/{cluster_id}/{shard}"),
            max_filter_peers: 3,
            max_push_peers: 3,
            filter_ping_interval: Duration::from_secs(60),
            request_timeout: Duration::from_secs(10),
            dial_timeout: Duration::from_secs(45),
            redial_delay: Duration::from_secs(5),
            max_redial_delay: Duration::from_secs(60),
            inbox_capacity: 2_000,
            agent_version: concat!("zknox-waku-light/", env!("CARGO_PKG_VERSION")).into(),
        }
    }
}

/// A message received through a filter subscription.
#[derive(Debug, Clone)]
pub struct Message {
    pub content_topic: String,
    pub payload: Vec<u8>,
    /// Nanoseconds since the epoch, when the sender set it.
    pub timestamp_ns: Option<u64>,
}

/// Outcome of one publish.
#[derive(Debug, Clone, Default)]
pub struct PublishReport {
    /// Peers that accepted the light push.
    pub accepted: usize,
    /// `peer (lightpush v3|v2)` for each acceptance.
    pub accepted_via: Vec<String>,
    /// `peer: reason` for each refusal or failure.
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub connected_peers: usize,
    /// Connected peers that announced filter and light push.
    pub service_peers: usize,
    /// Service nodes currently holding our filter subscription.
    pub filter_subscriptions: usize,
    /// Latest failure; cleared when a filter subscription succeeds.
    pub last_error: Option<String>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("libp2p setup failed: {0}")]
    Setup(String),
    #[error("no connected peer offers {0}")]
    NoPeer(&'static str),
    #[error("light push refused by every peer: {}", .0.join("; "))]
    Refused(Vec<String>),
    #[error("waku node stopped")]
    Stopped,
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    /// The Railgun fleet (nwaku, BearSSL) only speaks TLS 1.2 on wss. Without the `tls12` feature
    /// of rustls every dial ends in a `HandshakeFailure` alert.
    #[test]
    fn wss_client_offers_tls12() {
        assert!(
            rustls::DEFAULT_VERSIONS
                .iter()
                .any(|v| v.version == rustls::ProtocolVersion::TLSv1_2)
        );
    }
}

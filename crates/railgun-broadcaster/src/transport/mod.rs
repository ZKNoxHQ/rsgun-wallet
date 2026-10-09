//! Waku access, reduced to what the broadcaster protocol needs: publish a payload on a content
//! topic and drain what arrived on a set of content topics.
//!
//! Implementations: [`light::LightNodeTransport`] (feature `light-node`), a native light node on
//! rust-libp2p that dials the Railgun fleet itself; [`nwaku_rest::NwakuRest`], a local nwaku over
//! REST; [`bridge::BrowserBridge`], js-waku in a browser tab; an in-memory hub for the tests.

use async_trait::async_trait;
use thiserror::Error;

pub mod bridge;
#[cfg(feature = "light-node")]
pub mod light;
#[cfg(any(test, feature = "testing"))]
pub mod memory;
pub mod nwaku_rest;

#[derive(Debug, Clone)]
pub struct WakuMessage {
    pub content_topic: String,
    pub payload: Vec<u8>,
    /// Nanoseconds since the epoch when the node provides it.
    pub timestamp_ns: Option<u64>,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("waku node unreachable at {url}: {source}")]
    Unreachable {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("waku node answered {status}: {body}")]
    Node { status: u16, body: String },
    /// The Waku node runs elsewhere (wallet tab) and is absent or not ready.
    #[error("{0}")]
    Remote(String),
    #[error("malformed waku message: {0}")]
    Malformed(String),
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait WakuTransport: Send + Sync {
    /// Makes sure the node relays the Railgun shard. Idempotent.
    async fn subscribe(&self) -> Result<(), TransportError>;

    /// Returns the messages received since the previous call, on any content topic. The caller
    /// filters: one drain serves the fee monitor and the response wait.
    async fn poll(&self) -> Result<Vec<WakuMessage>, TransportError>;

    async fn publish(&self, content_topic: &str, payload: &[u8]) -> Result<(), TransportError>;

    /// Number of connected peers, when the node can tell. Diagnostic only.
    async fn peer_count(&self) -> Option<usize> {
        None
    }

    /// What happened to the publishes so far, when the transport knows. Compare two readings
    /// around a send: a request no Waku peer accepted looks, from the answer side, exactly like
    /// a broadcaster that stays silent.
    fn publish_stats(&self) -> Option<bridge::PublishStats> {
        None
    }
}

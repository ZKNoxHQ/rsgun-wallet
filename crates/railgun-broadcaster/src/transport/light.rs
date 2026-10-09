//! Native Waku light node (`waku-light`, rust-libp2p) dialling the Railgun fleet itself: filter
//! to receive, light push to send, over wss on `:8000`. Replaces the js-waku node of the wallet
//! tab ([`super::bridge`]) with nothing to keep open and no JavaScript.
//!
//! The node starts on the first call, on the caller's runtime (tokio natively, the page's event
//! loop in a browser), and stops when the transport is dropped.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use waku_light::{Config, LightNode, Multiaddr, Status};

use super::{TransportError, WakuMessage, WakuTransport, bridge::PublishStats};
use crate::wire::{CLUSTER_ID, FLEET_WSS_PEERS, SHARD_ID, fees_topic, transact_response_topic};

pub struct LightNodeTransport {
    config: Config,
    topics: Vec<String>,
    node: Mutex<Option<Arc<LightNode>>>,
    stats: Mutex<PublishStats>,
}

impl LightNodeTransport {
    /// Railgun fleet (wss peers of the reference web client), cluster 5 shard 1, listening on the
    /// fees and transact-response topics of `chain_id`.
    pub fn for_chain(chain_id: u64) -> Self {
        let bootstrap = FLEET_WSS_PEERS
            .iter()
            .map(|a| a.parse::<Multiaddr>().expect("valid fleet multiaddr"))
            .collect();
        Self::new(
            Config::new(bootstrap, CLUSTER_ID, SHARD_ID),
            vec![fees_topic(chain_id), transact_response_topic(chain_id)],
        )
    }

    pub fn new(config: Config, topics: Vec<String>) -> Self {
        Self { config, topics, node: Mutex::new(None), stats: Mutex::new(PublishStats::default()) }
    }

    /// State of the node, `None` before the first call started it.
    pub fn status(&self) -> Option<Status> {
        self.node.lock().unwrap().as_ref().map(|n| n.status())
    }

    fn node(&self) -> Result<Arc<LightNode>, TransportError> {
        let mut slot = self.node.lock().unwrap();
        if let Some(node) = slot.as_ref() {
            return Ok(node.clone());
        }
        let node = LightNode::start(self.config.clone())
            .map_err(|e| TransportError::Remote(format!("native Waku node: {e}")))?;
        node.subscribe(self.topics.iter().cloned());
        let node = Arc::new(node);
        *slot = Some(node.clone());
        Ok(node)
    }

    fn not_ready(status: &Status) -> TransportError {
        let mut text = format!(
            "native Waku node not ready: {} peer(s) connected, {} service node(s), {} filter subscription(s)",
            status.connected_peers, status.service_peers, status.filter_subscriptions
        );
        if let Some(e) = &status.last_error {
            text.push_str(&format!("; last error: {e}"));
        }
        TransportError::Remote(text)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl WakuTransport for LightNodeTransport {
    async fn subscribe(&self) -> Result<(), TransportError> {
        let node = self.node()?;
        node.subscribe(self.topics.iter().cloned());
        let status = node.status();
        if status.filter_subscriptions == 0 {
            return Err(Self::not_ready(&status));
        }
        Ok(())
    }

    async fn poll(&self) -> Result<Vec<WakuMessage>, TransportError> {
        let node = self.node()?;
        let status = node.status();
        if status.filter_subscriptions == 0 {
            return Err(Self::not_ready(&status));
        }
        Ok(node
            .drain()
            .into_iter()
            .map(|m| WakuMessage {
                content_topic: m.content_topic,
                payload: m.payload,
                timestamp_ns: m.timestamp_ns,
            })
            .collect())
    }

    async fn publish(&self, content_topic: &str, payload: &[u8]) -> Result<(), TransportError> {
        self.stats.lock().unwrap().queued += 1;
        let outcome = match self.node() {
            Ok(node) => node
                .publish(content_topic, payload)
                .await
                .map_err(|e| TransportError::Remote(format!("native Waku node: {e}"))),
            Err(e) => Err(e),
        };
        let mut stats = self.stats.lock().unwrap();
        match outcome {
            Ok(report) => {
                stats.delivered += 1;
                tracing::info!(
                    "light push accepted by {} peer(s){}",
                    report.accepted,
                    if report.failures.is_empty() {
                        String::new()
                    } else {
                        format!(", refused by {}: {}", report.failures.len(), report.failures.join("; "))
                    }
                );
                Ok(())
            }
            Err(e) => {
                stats.failed += 1;
                stats.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    fn publish_stats(&self) -> Option<PublishStats> {
        Some(self.stats.lock().unwrap().clone())
    }

    async fn peer_count(&self) -> Option<usize> {
        self.status().map(|s| s.connected_peers)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn fleet_config() {
        let t = LightNodeTransport::for_chain(11_155_111);
        assert_eq!(t.config.bootstrap.len(), FLEET_WSS_PEERS.len());
        assert_eq!(t.config.pubsub_topic, crate::wire::PUBSUB_TOPIC);
        assert_eq!(
            t.topics,
            vec![
                "/railgun/v2/0-11155111-fees/json".to_string(),
                "/railgun/v2/0-11155111-transact-response/json".to_string()
            ]
        );
        assert!(t.status().is_none(), "the node starts on first use only");
    }

    /// Nothing listens there: every call reports the node as not ready instead of blocking, and
    /// publish finds no peer.
    #[tokio::test]
    async fn not_ready_without_peers() {
        let addr: Multiaddr =
            "/ip4/127.0.0.1/tcp/9/ws/p2p/16Uiu2HAmFbD2ZvAFi2j9jjDo6g4HFbQAhfjDfnTTrbyRGQRmtG7x".parse().unwrap();
        let t = LightNodeTransport::new(Config::new(vec![addr], 5, 1), vec!["/t/1/a/json".into()]);
        assert!(matches!(t.subscribe().await, Err(TransportError::Remote(_))));
        assert!(matches!(t.poll().await, Err(TransportError::Remote(_))));
        assert!(matches!(t.publish("/t/1/b/json", b"x").await, Err(TransportError::Remote(_))));
        assert_eq!(t.peer_count().await, Some(0));
        let stats = t.publish_stats().unwrap();
        assert_eq!((stats.queued, stats.delivered, stats.failed), (1, 0, 1));
        assert!(stats.last_error.is_some());
    }
}

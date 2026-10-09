//! nwaku REST API client (`/relay/v1/*`), static sharding.
//!
//! The Railgun content topics do not follow the autosharding naming scheme, so the named pubsub
//! topic endpoints are used rather than `/relay/v1/auto/*`.

use crate::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use super::{TransportError, WakuMessage, WakuTransport};
use crate::wire::PUBSUB_TOPIC;

pub struct NwakuRest {
    base: String,
    pubsub_topic: String,
    http: reqwest::Client,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestMessage {
    payload: String,
    content_topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timestamp: Option<u64>,
}

impl NwakuRest {
    /// `base` is the REST root of the node, for instance `http://127.0.0.1:8645`.
    pub fn new(base: impl Into<String>) -> Self {
        Self::with_pubsub_topic(base, PUBSUB_TOPIC)
    }

    pub fn with_pubsub_topic(base: impl Into<String>, pubsub_topic: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            pubsub_topic: pubsub_topic.into(),
            http: reqwest::Client::new(),
        }
    }

    fn topic_path(&self) -> String {
        // The pubsub topic goes in the path, percent-encoded ("/" -> %2F).
        self.pubsub_topic.replace('/', "%2F")
    }

    fn unreachable(&self, e: reqwest::Error) -> TransportError {
        TransportError::Unreachable {
            url: self.base.clone(),
            source: Box::new(e),
        }
    }

    async fn check(&self, response: reqwest::Response) -> Result<reqwest::Response, TransportError> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Err(TransportError::Node { status, body })
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl WakuTransport for NwakuRest {
    async fn subscribe(&self) -> Result<(), TransportError> {
        let response = self
            .http
            .post(format!("{}/relay/v1/subscriptions", self.base))
            .json(&[&self.pubsub_topic])
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        self.check(response).await.map(|_| ())
    }

    async fn poll(&self) -> Result<Vec<WakuMessage>, TransportError> {
        let response = self
            .http
            .get(format!("{}/relay/v1/messages/{}", self.base, self.topic_path()))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        let messages: Vec<RestMessage> = self
            .check(response)
            .await?
            .json()
            .await
            .map_err(|e| TransportError::Malformed(e.to_string()))?;

        Ok(messages
            .into_iter()
            .filter_map(|m| {
                let payload = STANDARD.decode(m.payload.as_bytes()).ok()?;
                Some(WakuMessage {
                    content_topic: m.content_topic,
                    payload,
                    timestamp_ns: m.timestamp,
                })
            })
            .collect())
    }

    async fn publish(&self, content_topic: &str, payload: &[u8]) -> Result<(), TransportError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_nanos() as u64);
        let response = self
            .http
            .post(format!("{}/relay/v1/messages/{}", self.base, self.topic_path()))
            .json(&RestMessage {
                payload: STANDARD.encode(payload),
                content_topic: content_topic.to_string(),
                timestamp,
            })
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        self.check(response).await.map(|_| ())
    }

    async fn peer_count(&self) -> Option<usize> {
        let response = self
            .http
            .get(format!("{}/admin/v1/peers", self.base))
            .send()
            .await
            .ok()?;
        let peers: Vec<serde_json::Value> = response.json().await.ok()?;
        Some(peers.len())
    }
}

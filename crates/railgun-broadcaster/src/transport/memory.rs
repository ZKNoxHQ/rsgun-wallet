//! In-memory Waku hub: every handle sees what the other handles publish.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{TransportError, WakuMessage, WakuTransport};

#[derive(Default)]
struct Hub {
    /// One inbox per handle.
    inboxes: Vec<Vec<WakuMessage>>,
}

#[derive(Clone, Default)]
pub struct MemoryHub(Arc<Mutex<Hub>>);

pub struct MemoryTransport {
    hub: MemoryHub,
    index: usize,
}

impl MemoryHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&self) -> MemoryTransport {
        let mut hub = self.0.lock().unwrap();
        hub.inboxes.push(Vec::new());
        MemoryTransport {
            hub: self.clone(),
            index: hub.inboxes.len() - 1,
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl WakuTransport for MemoryTransport {
    async fn subscribe(&self) -> Result<(), TransportError> {
        Ok(())
    }

    async fn poll(&self) -> Result<Vec<WakuMessage>, TransportError> {
        Ok(std::mem::take(&mut self.hub.0.lock().unwrap().inboxes[self.index]))
    }

    async fn publish(&self, content_topic: &str, payload: &[u8]) -> Result<(), TransportError> {
        let mut hub = self.hub.0.lock().unwrap();
        for (i, inbox) in hub.inboxes.iter_mut().enumerate() {
            if i != self.index {
                inbox.push(WakuMessage {
                    content_topic: content_topic.to_string(),
                    payload: payload.to_vec(),
                    timestamp_ns: None,
                });
            }
        }
        Ok(())
    }

    async fn peer_count(&self) -> Option<usize> {
        Some(self.hub.0.lock().unwrap().inboxes.len().saturating_sub(1))
    }
}

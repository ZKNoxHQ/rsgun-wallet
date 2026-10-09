//! Deterministic message hash (RFC 14/WAKU2-MESSAGE) and a bounded seen-set for deduplication.

use std::collections::{HashSet, VecDeque};

use sha2::{Digest, Sha256};

use crate::proto::WakuMessage;

pub fn message_hash(pubsub_topic: &str, m: &WakuMessage) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(pubsub_topic.as_bytes());
    h.update(&m.payload);
    h.update(m.content_topic.as_bytes());
    if let Some(meta) = &m.meta {
        h.update(meta);
    }
    h.update(m.timestamp.unwrap_or(0).to_be_bytes());
    h.finalize().into()
}

pub struct SeenSet {
    order: VecDeque<[u8; 32]>,
    set: HashSet<[u8; 32]>,
    capacity: usize,
}

impl SeenSet {
    pub fn new(capacity: usize) -> Self {
        Self { order: VecDeque::new(), set: HashSet::new(), capacity }
    }

    /// True the first time `hash` is seen.
    pub fn insert(&mut self, hash: [u8; 32]) -> bool {
        if !self.set.insert(hash) {
            return false;
        }
        self.order.push_back(hash);
        if self.order.len() > self.capacity
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_and_eviction() {
        let mut s = SeenSet::new(2);
        assert!(s.insert([1; 32]));
        assert!(!s.insert([1; 32]));
        assert!(s.insert([2; 32]));
        assert!(s.insert([3; 32]));
        assert!(s.insert([1; 32]), "evicted, so new again");
    }

    #[test]
    fn hash_depends_on_topic_and_timestamp() {
        let m = WakuMessage { payload: vec![1], content_topic: "/a/1/b/c".into(), timestamp: Some(5), ..Default::default() };
        let mut m2 = m.clone();
        m2.timestamp = Some(6);
        assert_ne!(message_hash("/waku/2/rs/5/1", &m), message_hash("/waku/2/rs/5/2", &m));
        assert_ne!(message_hash("/waku/2/rs/5/1", &m), message_hash("/waku/2/rs/5/1", &m2));
    }
}

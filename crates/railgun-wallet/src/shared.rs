//! State shared between the HTTP layer and the engine thread.
//!
//! The engine owns the `RailgunProvider` and processes commands one at a time. Everything the
//! front needs to render is pushed into this snapshot so that GET endpoints never wait on a
//! running proof or sync.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, RwLock},
};

// `std::time::SystemTime::now()` panics on wasm32-unknown-unknown (no clock); `web-time` is a
// drop-in that reads the browser clock there and falls back to `std` on native.
#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_arch = "wasm32")]
use web_time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context};

const MAX_LOG_LINES: usize = 2000;
const MAX_JOBS: usize = 200;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TransportInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub enabled: bool,
    pub note: String,
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    pub unlocked: bool,
    pub chain_id: Option<u64>,
    pub address: Option<String>,
    pub derivation: Option<String>,
    pub eoa: Option<String>,
    /// Where the public account comes from: a derivation path, or "imported key".
    pub eoa_source: Option<String>,
    pub eoa_balance: Option<String>,
    /// The public account is the Ledger's Ethereum app (its address may not be known yet).
    pub eoa_ledger: bool,
    pub poi: bool,
    /// POI lists this session proves against; broadcasters requiring another are unusable.
    pub poi_list_keys: Vec<String>,
    pub synced_block: Option<u64>,
    pub wrapped_base_token: Option<String>,
    pub railgun_smart_wallet: Option<String>,
    pub bundler_url: Option<String>,
    pub transports: Vec<TransportInfo>,
    pub balances: Vec<Value>,
    pub notes: Vec<Value>,
    pub poi_pending: Vec<Value>,
    pub updated_at: u64,
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: u64,
    pub kind: String,
    pub state: JobState,
    pub created_at: u64,
    pub finished_at: Option<u64>,
    pub steps: Vec<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Jobs {
    next: u64,
    list: VecDeque<Job>,
}

impl Jobs {
    pub fn create(&mut self, kind: &str) -> u64 {
        self.next += 1;
        let id = self.next;
        self.list.push_back(Job {
            id,
            kind: kind.to_string(),
            state: JobState::Queued,
            created_at: now_ms(),
            finished_at: None,
            steps: Vec::new(),
            result: None,
            error: None,
        });
        while self.list.len() > MAX_JOBS {
            self.list.pop_front();
        }
        id
    }

    pub fn update(&mut self, id: u64, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.list.iter_mut().find(|j| j.id == id) {
            f(job);
        }
    }

    pub fn get(&self, id: u64) -> Option<Job> {
        self.list.iter().find(|j| j.id == id).cloned()
    }

    pub fn all(&self) -> Vec<Job> {
        self.list.iter().rev().cloned().collect()
    }

    pub fn active(&self) -> Option<Job> {
        self.list
            .iter()
            .find(|j| j.state == JobState::Running)
            .cloned()
    }
}

#[derive(Clone, Serialize)]
pub struct LogLine {
    pub seq: u64,
    pub ts: u64,
    pub level: String,
    pub target: String,
    pub msg: String,
}

#[derive(Default)]
pub struct Logs {
    seq: u64,
    lines: VecDeque<LogLine>,
}

impl Logs {
    pub fn since(&self, seq: u64) -> Vec<LogLine> {
        self.lines.iter().filter(|l| l.seq > seq).cloned().collect()
    }
}

/// Live view of the broadcaster network, written by the fee monitor task.
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LegacyStatus {
    /// "native": light node in the daemon. "browser": js-waku in the wallet tab. "nwaku": local
    /// node over REST.
    pub mode: Option<String>,
    pub waku_url: Option<String>,
    pub reachable: bool,
    pub peers: Option<usize>,
    pub quotes: Vec<Value>,
    /// Number of trusted fee signers configured, 0 when offers are not capped.
    pub trusted_signers: usize,
    /// Authorized rate per token, from the trusted signers' live announcements.
    pub authorized_fees: Vec<Value>,
    pub error: Option<String>,
    pub updated_at: u64,
}

#[derive(Default)]
pub struct Shared {
    /// Link to the Waku node running in the wallet tab. One per process: the tab does not know
    /// about sessions.
    pub bridge: Arc<railgun_broadcaster::BrowserBridge>,
    pub legacy: RwLock<LegacyStatus>,
    pub status: RwLock<StatusSnapshot>,
    pub jobs: Mutex<Jobs>,
    pub logs: Mutex<Logs>,
}

pub type SharedRef = Arc<Shared>;

/// Tracing layer that mirrors log events into [`Shared::logs`] for the front.
pub struct FrontLogLayer(pub SharedRef);

struct MsgVisitor(String);

impl Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
}

impl<S: Subscriber> Layer<S> for FrontLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let target = meta.target();
        // Only our own crates: dependency chatter (hyper, reqwest, libp2p...) stays on stderr.
        if !(target.starts_with("railgun")
            || target.starts_with("userop_kit")
            || target.starts_with("waku_light"))
        {
            return;
        }
        let mut visitor = MsgVisitor(String::new());
        event.record(&mut visitor);
        // Proofs and calldata are large, keep the front log readable.
        let mut msg = visitor.0;
        if msg.len() > 600 {
            let mut cut = 600;
            while !msg.is_char_boundary(cut) {
                cut -= 1;
            }
            msg.truncate(cut);
            msg.push_str(" …");
        }
        if let Ok(mut logs) = self.0.logs.lock() {
            logs.seq += 1;
            let seq = logs.seq;
            logs.lines.push_back(LogLine {
                seq,
                ts: now_ms(),
                level: meta.level().to_string(),
                target: target.to_string(),
                msg,
            });
            while logs.lines.len() > MAX_LOG_LINES {
                logs.lines.pop_front();
            }
        }
    }
}

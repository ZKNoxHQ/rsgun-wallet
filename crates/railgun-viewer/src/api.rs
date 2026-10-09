//! HTTP API on loopback plus the embedded front.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    extract::State,
    http::header,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use railgun_broadcaster::{BroadcasterClient, BrowserBridge, WakuTransport};
use serde_json::{Value, json};

use crate::{
    engine::{Engine, UnlockParams},
    health::{self, HealthParams},
    shared::{self, SharedRef, StatusView, now_ms},
    waku_link,
};

/// js-waku light node, the wallet's build (`crates/railgun-wallet/waku-bridge`), served to the
/// Network tab so the viewer sees broadcasters without a local nwaku node.
const WAKU_BUNDLE_JS: &str = include_str!("../../railgun-wallet/static/waku-bundle.js");

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub shared: SharedRef,
    pub version: &'static str,
    pub data_dir: Arc<String>,
    /// Waku link with the tab (same bridge as the wallet).
    pub bridge: Arc<BrowserBridge>,
    /// One broadcaster client per chain over that bridge, created on first use.
    pub bridge_clients: Arc<Mutex<HashMap<u64, Arc<BroadcasterClient>>>>,
    /// One broadcaster client per chain over the viewer's own Rust Waku light node
    /// (`LightNodeTransport`). The node starts on the first probe and stays connected.
    pub native_clients: Arc<Mutex<HashMap<u64, Arc<BroadcasterClient>>>>,
}

impl AppState {
    pub fn bridge_client(&self, chain_id: u64) -> Arc<BroadcasterClient> {
        let mut map = self.bridge_clients.lock().expect("bridge clients");
        map.entry(chain_id)
            .or_insert_with(|| {
                let transport: Arc<dyn WakuTransport> = self.bridge.clone();
                Arc::new(BroadcasterClient::new(transport, chain_id))
            })
            .clone()
    }

    pub fn native_client(&self, chain_id: u64) -> Arc<BroadcasterClient> {
        let mut map = self.native_clients.lock().expect("native clients");
        map.entry(chain_id)
            .or_insert_with(|| {
                let transport: Arc<dyn WakuTransport> =
                    Arc::new(railgun_broadcaster::LightNodeTransport::for_chain(chain_id));
                Arc::new(BroadcasterClient::new(transport, chain_id))
            })
            .clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/snapshot", get(snapshot))
        .route("/api/unlock", post(unlock))
        .route("/api/sync", post(sync))
        .route("/api/refresh", post(refresh))
        .route("/api/lock", post(lock))
        .route("/api/derive", post(derive))
        .route("/api/health", get(health_get))
        .route("/api/health/run", post(health_run))
        .route("/waku-bundle.js", get(waku_bundle))
        .route("/api/defaults", get(defaults))
        .route("/api/waku/exchange", post(waku_exchange))
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    Html(include_str!("../static/index.html"))
}

async fn status(State(st): State<AppState>) -> Json<StatusView> {
    Json(shared::status_view(&st.shared, st.version, st.data_dir.to_string()))
}

async fn snapshot(State(st): State<AppState>) -> Json<Value> {
    let s = st.shared.lock().expect("shared");
    Json(s.snapshot.clone().unwrap_or(Value::Null))
}

async fn unlock(State(st): State<AppState>, Json(p): Json<UnlockParams>) -> Json<Value> {
    match st.engine.unlock(p).await {
        Ok(()) => Json(json!({ "ok": true })),
        Err(e) => Json(json!({ "ok": false, "error": format!("{e:#}") })),
    }
}

async fn sync(State(st): State<AppState>) -> Json<Value> {
    match st.engine.sync() {
        Ok(()) => Json(json!({ "ok": true })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn refresh(State(st): State<AppState>) -> Json<Value> {
    match st.engine.refresh() {
        Ok(()) => Json(json!({ "ok": true })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn waku_bundle() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], WAKU_BUNDLE_JS)
}

async fn defaults() -> Json<Value> {
    Json(waku_link::defaults(true))
}

/// One round trip of the tab's Waku node, as in the wallet: received messages in, publishes out.
async fn waku_exchange(State(st): State<AppState>, Json(body): Json<waku_link::ExchangeBody>) -> Json<Value> {
    Json(waku_link::exchange(&st.bridge, body))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeriveBody {
    mnemonic: String,
    #[serde(default)]
    derivation: crate::wallet_keys::Derivation,
    #[serde(default)]
    index: u32,
    #[serde(default)]
    chain_id: Option<u64>,
}

/// Keys derived from a mnemonic, without touching the engine: the viewing private key (what the
/// view-only mode takes) and the 0zk address. Sepolia test seeds only, as the form says.
async fn derive(Json(b): Json<DeriveBody>) -> Json<Value> {
    Json(crate::keys::derive_display(&b.mnemonic, b.derivation, b.index, b.chain_id))
}

async fn health_get(State(st): State<AppState>) -> Json<Value> {
    let s = st.shared.lock().expect("shared");
    Json(json!({ "running": s.health_running, "report": s.health }))
}

/// Starts the probes in the background; poll `GET /api/health` for the report.
async fn health_run(State(st): State<AppState>, Json(p): Json<HealthParams>) -> Json<Value> {
    {
        let mut s = st.shared.lock().expect("shared");
        if s.health_running {
            return Json(json!({ "ok": false, "error": "already running" }));
        }
        s.health_running = true;
        s.updated_at = now_ms();
    }
    let shared = st.shared.clone();
    let bridge = st.bridge_client(p.chain_id);
    let native = st.native_client(p.chain_id);
    tokio::spawn(async move {
        let report = health::run(p, Some(bridge), Some(native)).await;
        if let Ok(mut s) = shared.lock() {
            s.health = serde_json::to_value(&report).ok();
            s.health_running = false;
            s.updated_at = now_ms();
            s.log.push(format!(
                "health: rpc {} · subsquid {} · poi {} · broadcasters {}",
                report.rpc.level, report.subsquid.level, report.poi.level, report.broadcasters.level
            ));
        }
    });
    Json(json!({ "ok": true }))
}

async fn lock(State(st): State<AppState>) -> Json<Value> {
    match st.engine.lock().await {
        Ok(()) => Json(json!({ "ok": true })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

//! The daemon's engine and HTTP routes, for the browser: one actor task (commands run in order,
//! as on the daemon's engine thread), the same `/api/…` routes as `railgun-viewer/src/api.rs`,
//! the SDK database and the viewer cache in IndexedDB.

use std::{cell::RefCell, collections::HashMap, sync::Arc};

use alloy::{
    network::Ethereum,
    providers::{Provider, ProviderBuilder},
};
use anyhow::{Context, Result, anyhow, bail};
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use kv_store_js::JsKvStore;
use railgun::{builder::RailgunBuilder, chain_config::ChainConfig};
use railgun_broadcaster::{BroadcasterClient, BrowserBridge, WakuTransport};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::spawn_local;

use crate::{
    health::{self, HealthParams},
    keys,
    session::{self, CacheStore, Caches, Session, UnlockParams},
    shared::{self, SharedRef, log, now_ms},
    waku_link,
    wallet_keys::Derivation,
};

const VERSION: &str = concat!("web ", env!("CARGO_PKG_VERSION"));
const STORAGE: &str = "this browser (IndexedDB)";

#[wasm_bindgen(module = "/js/idb.js")]
extern "C" {
    #[wasm_bindgen(js_name = openDb)]
    fn open_db(name: &str) -> JsValue;
    #[wasm_bindgen(js_name = cacheLoad, catch)]
    async fn cache_load(name: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_name = cacheSave)]
    fn cache_save(name: &str, json: &str) -> js_sys::Promise;
}

enum Command {
    Unlock(Box<UnlockParams>, oneshot::Sender<Result<()>>),
    Sync,
    Refresh,
    Lock(oneshot::Sender<()>),
}

thread_local! {
    static SHARED: SharedRef = shared::new_shared();
    static TX: RefCell<Option<mpsc::UnboundedSender<Command>>> = const { RefCell::new(None) };
    static BRIDGE: Arc<BrowserBridge> = Arc::new(BrowserBridge::new());
    static CLIENTS: RefCell<HashMap<u64, Arc<BroadcasterClient>>> = RefCell::new(HashMap::new());
    /// One broadcaster client per chain over the Rust Waku light node running in this worker
    /// (waku-light over the browser's WebSocket). The node starts on the first probe and stays up.
    static NATIVE: RefCell<HashMap<u64, Arc<BroadcasterClient>>> = RefCell::new(HashMap::new());
}

fn shared_ref() -> SharedRef {
    SHARED.with(Clone::clone)
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfigBuilder::new()
            .set_max_level(tracing::Level::WARN)
            .build(),
    );
    let (tx, mut rx) = mpsc::unbounded::<Command>();
    TX.with(|t| *t.borrow_mut() = Some(tx));
    spawn_local(async move {
        let mut actor = Actor { shared: shared_ref(), session: None };
        while let Some(cmd) = rx.next().await {
            actor.handle(cmd).await;
        }
    });
}

fn send(cmd: Command) -> Result<()> {
    TX.with(|t| {
        t.borrow()
            .as_ref()
            .ok_or_else(|| anyhow!("engine not started"))?
            .unbounded_send(cmd)
            .map_err(|_| anyhow!("engine task is gone"))
    })
}

/// IndexedDB copy of the viewer cache, written without waiting (the next sync rewrites it).
struct IdbCache {
    name: String,
}

impl CacheStore for IdbCache {
    fn save(&self, value: Value) {
        let _ = cache_save(&self.name, &value.to_string());
    }
}

struct Actor {
    shared: SharedRef,
    session: Option<Session>,
}

impl Actor {
    async fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Unlock(p, reply) => {
                let sync_now = p.sync_now;
                let accepted = p.precheck();
                if accepted.is_err() {
                    log(&self.shared, p.lengths());
                }
                let _ = reply.send(accepted.as_ref().map(|_| ()).map_err(|e| anyhow!("{e:#}")));
                if let Err(e) = accepted {
                    self.set_error(format!("unlock: {e:#}"));
                    return;
                }
                self.set_stage(Some("unlocking"));
                match self.unlock(*p).await {
                    Err(e) => self.set_error(format!("unlock: {e:#}")),
                    Ok(()) => {
                        self.set_stage(None);
                        if sync_now && self.session.is_some() {
                            self.sync().await;
                        }
                    }
                }
                self.set_stage(None);
            }
            Command::Sync => self.sync().await,
            Command::Refresh => self.refresh_snapshot().await,
            Command::Lock(reply) => {
                self.session = None;
                if let Ok(mut s) = self.shared.lock() {
                    *s = Default::default();
                    s.updated_at = now_ms();
                }
                let _ = reply.send(());
            }
        }
    }

    fn set_stage(&self, stage: Option<&'static str>) {
        if let Ok(mut s) = self.shared.lock() {
            s.stage = stage;
            s.updated_at = now_ms();
        }
    }

    fn set_error(&self, msg: String) {
        tracing::error!("{msg}");
        if let Ok(mut s) = self.shared.lock() {
            s.last_error = Some(msg.clone());
            s.log.push(format!("✗ {msg}"));
            s.updated_at = now_ms();
        }
    }

    async fn unlock(&mut self, p: UnlockParams) -> Result<()> {
        let chain = ChainConfig::from_chain_id(p.chain_id)
            .ok_or_else(|| anyhow!("unsupported chain id {}", p.chain_id))?;
        let resolved = keys::resolve(&p.credentials())?;
        let mode = resolved.mode();
        let rpc_url = p.rpc_url(&chain)?;
        let provider = ProviderBuilder::new()
            .network::<Ethereum>()
            .connect(&rpc_url)
            .await
            .context("connecting to RPC")?
            .erased();
        let rpc_chain = provider
            .get_chain_id()
            .await
            .context("eth_chainId (the RPC must allow requests from this page: CORS)")?;
        if rpc_chain != chain.id {
            bail!("RPC is on chain {rpc_chain}, expected {}", chain.id);
        }

        let (signer, _scheme) =
            session::make_signer(resolved, keys::non_empty(&p.address), &chain, &provider, &self.shared).await?;
        let address = signer.address();

        // One IndexedDB database per chain and account, named like the daemon's data directory.
        let tag = hex::encode(&Sha256::digest(address.to_string().as_bytes())[..8]);
        let name = format!("drrail-{}-{tag}", chain.id);
        let db = JsKvStore::store(open_db(&name).unchecked_into());

        let mut builder = RailgunBuilder::new(chain.clone(), provider.clone()).with_database(db);
        if p.poi {
            builder = if mode == "full" {
                builder.with_poi()
            } else {
                builder.with_poi_read_only()
            };
        }
        let mut railgun = builder.build().await.context("building railgun provider")?;
        railgun.register(signer.clone()).await?;

        let poi_endpoint = chain.poi_endpoint.clone();
        let caches = match cache_load(&name).await {
            Ok(v) => v
                .as_string()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .map(|v| Caches::from_value(&v))
                .unwrap_or_default(),
            Err(_) => Caches::default(),
        };
        log(
            &self.shared,
            format!(
                "✓ {mode} · {address} · chain {} · POI {} · stored in {STORAGE}",
                chain.id,
                if p.poi { "on" } else { "off" }
            ),
        );
        self.session = Some(Session::new(
            chain,
            provider,
            railgun,
            signer,
            mode,
            Box::new(IdbCache { name }),
            caches,
        ));
        if let Ok(mut s) = self.shared.lock() {
            s.unlocked = true;
            s.address = Some(address.to_string());
            s.mode = Some(mode);
            s.chain_id = Some(p.chain_id);
            s.last_error = None;
            s.updated_at = now_ms();
        }
        if p.poi {
            session::warn_if_poi_unreachable(p.chain_id, &poi_endpoint, &self.shared).await;
        }
        Ok(())
    }

    async fn sync(&mut self) {
        let Some(session) = self.session.as_mut() else {
            self.set_error("sync: not unlocked".into());
            return;
        };
        if let Ok(mut s) = self.shared.lock() {
            s.syncing = true;
            s.updated_at = now_ms();
        }
        log(&self.shared, "sync: scanning commitments, nullifiers and POI statuses…");
        match session.railgun.sync().await {
            Ok(()) => log(&self.shared, format!("✓ synced to block {}", session.railgun.synced_block())),
            Err(e) => self.set_error(format!("sync failed: {e:#}")),
        }
        self.refresh_snapshot().await;
        if let Ok(mut s) = self.shared.lock() {
            s.syncing = false;
            s.updated_at = now_ms();
        }
    }

    async fn refresh_snapshot(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let result = session::build_history(session).await;
        for e in session.squid.errors() {
            log(&self.shared, format!("✗ {e}"));
        }
        match result {
            Ok(h) => {
                if let Ok(mut s) = self.shared.lock() {
                    s.synced_block = Some(session.railgun.synced_block());
                    s.snapshot = serde_json::to_value(&h).ok();
                    s.updated_at = now_ms();
                }
                log(
                    &self.shared,
                    format!(
                        "history: {} transaction(s), {} note(s), {} emitted without POI, {} pending",
                        h.transactions.len(),
                        h.notes.len(),
                        h.missing_poi.len(),
                        h.pending_poi.len()
                    ),
                );
            }
            Err(e) => self.set_error(format!("history: {e:#}")),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeriveBody {
    mnemonic: String,
    #[serde(default)]
    derivation: Derivation,
    #[serde(default)]
    index: u32,
    #[serde(default)]
    chain_id: Option<u64>,
}

fn bridge_client(chain_id: u64) -> Arc<BroadcasterClient> {
    CLIENTS.with(|c| {
        c.borrow_mut()
            .entry(chain_id)
            .or_insert_with(|| {
                let transport: Arc<dyn WakuTransport> = BRIDGE.with(Clone::clone);
                Arc::new(BroadcasterClient::new(transport, chain_id))
            })
            .clone()
    })
}

fn native_client(chain_id: u64) -> Arc<BroadcasterClient> {
    NATIVE.with(|c| {
        c.borrow_mut()
            .entry(chain_id)
            .or_insert_with(|| {
                let transport: Arc<dyn WakuTransport> =
                    Arc::new(railgun_broadcaster::LightNodeTransport::for_chain(chain_id));
                Arc::new(BroadcasterClient::new(transport, chain_id))
            })
            .clone()
    })
}

fn ok_or_error(r: Result<()>) -> Value {
    match r {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T> {
    serde_json::from_str(if body.trim().is_empty() { "{}" } else { body }).context("request body")
}

async fn route(method: &str, path: &str, body: &str) -> Value {
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        (_, "/api/status") => serde_json::to_value(shared::status_view(&shared_ref(), VERSION, STORAGE.into()))
            .unwrap_or(Value::Null),
        (_, "/api/snapshot") => shared_ref()
            .lock()
            .ok()
            .and_then(|s| s.snapshot.clone())
            .unwrap_or(Value::Null),
        ("POST", "/api/unlock") => {
            let p: UnlockParams = match parse(body) {
                Ok(p) => p,
                Err(e) => return json!({ "ok": false, "error": format!("{e:#}") }),
            };
            let (tx, rx) = oneshot::channel();
            if let Err(e) = send(Command::Unlock(Box::new(p), tx)) {
                return ok_or_error(Err(e));
            }
            ok_or_error(rx.await.unwrap_or_else(|_| Err(anyhow!("engine dropped the request"))))
        }
        ("POST", "/api/sync") => ok_or_error(send(Command::Sync)),
        ("POST", "/api/refresh") => ok_or_error(send(Command::Refresh)),
        ("POST", "/api/lock") => {
            let (tx, rx) = oneshot::channel();
            if let Err(e) = send(Command::Lock(tx)) {
                return ok_or_error(Err(e));
            }
            ok_or_error(rx.await.map_err(|_| anyhow!("engine dropped the request")))
        }
        ("POST", "/api/derive") => match parse::<DeriveBody>(body) {
            Ok(b) => keys::derive_display(&b.mnemonic, b.derivation, b.index, b.chain_id),
            Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
        },
        (_, "/api/health") => {
            let sh = shared_ref();
            let s = sh.lock().expect("shared");
            json!({ "running": s.health_running, "report": s.health })
        }
        ("POST", "/api/health/run") => {
            let p: HealthParams = match parse(body) {
                Ok(p) => p,
                Err(e) => return json!({ "ok": false, "error": format!("{e:#}") }),
            };
            let sh = shared_ref();
            {
                let mut s = sh.lock().expect("shared");
                if s.health_running {
                    return json!({ "ok": false, "error": "already running" });
                }
                s.health_running = true;
                s.updated_at = now_ms();
            }
            let client = bridge_client(p.chain_id);
            let native = native_client(p.chain_id);
            spawn_local(async move {
                let report = health::run(p, Some(client), Some(native)).await;
                if let Ok(mut s) = sh.lock() {
                    s.health = serde_json::to_value(&report).ok();
                    s.health_running = false;
                    s.updated_at = now_ms();
                    s.log.push(format!(
                        "health: rpc {} · subsquid {} · poi {} · broadcasters {}",
                        report.rpc.level, report.subsquid.level, report.poi.level, report.broadcasters.level
                    ));
                }
            });
            json!({ "ok": true })
        }
        (_, "/api/defaults") => waku_link::defaults(true),
        ("POST", "/api/waku/exchange") => match parse::<waku_link::ExchangeBody>(body) {
            Ok(b) => BRIDGE.with(|br| waku_link::exchange(br, b)),
            Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
        },
        _ => json!({ "ok": false, "error": format!("no route {method} {path}") }),
    }
}

/// Entry point used by `web/worker.js`: one call per `/api/…` request of the front, JSON text in
/// and out.
#[wasm_bindgen]
pub async fn api(method: String, path: String, body: String) -> String {
    route(&method.to_ascii_uppercase(), &path, &body).await.to_string()
}

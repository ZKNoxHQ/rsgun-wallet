//! Transport-free dispatch of the front's API.
//!
//! `api.rs` wraps it in axum for the desktop daemon; the Android app calls `dispatch` from a
//! Tauri command. Same paths, same JSON bodies, so the front runs unchanged on both.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::{
    engine::{Command, Op, UnlockParams},
    shared::SharedRef,
};

#[derive(Clone)]
pub struct Ctx {
    pub shared: SharedRef,
    pub engine: mpsc::Sender<Command>,
}

#[derive(Debug)]
pub struct ApiError {
    /// HTTP status the daemon answers with. The mobile shim maps it back onto a `Response`.
    pub code: u16,
    pub message: String,
}

impl ApiError {
    fn new(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

const BAD_REQUEST: u16 = 400;
const NOT_FOUND: u16 = 404;
const CONFLICT: u16 = 409;
const INTERNAL: u16 = 500;
const UNAVAILABLE: u16 = 503;

fn gone() -> ApiError {
    ApiError::new(UNAVAILABLE, "engine stopped")
}

fn poisoned() -> ApiError {
    ApiError::new(INTERNAL, "state poisoned")
}

/// Routes one call. `body` is the POST body, or the query parameters of a GET, or `None`.
pub async fn dispatch(ctx: &Ctx, path: &str, body: Option<Value>) -> Result<Value, ApiError> {
    match path {
        "/api/defaults" => Ok(defaults()),
        "/api/status" => status(ctx),
        "/api/unlock" => unlock(ctx, body).await,
        "/api/lock" => lock(ctx).await,
        "/api/empty-cache" => empty_cache(ctx).await,
        "/api/op" => op(ctx, body).await,
        "/api/jobs" => jobs(ctx),
        "/api/logs" => logs(ctx, body),
        "/api/waku/exchange" => waku_exchange(ctx, body),
        other => match other.strip_prefix("/api/jobs/") {
            Some(id) => {
                let id: u64 = id
                    .parse()
                    .map_err(|_| ApiError::new(NOT_FOUND, "unknown job"))?;
                job(ctx, id)
            }
            None => Err(ApiError::new(NOT_FOUND, "unknown endpoint")),
        },
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: Option<Value>) -> Result<T, ApiError> {
    serde_json::from_value(body.unwrap_or(Value::Null))
        .map_err(|e| ApiError::new(BAD_REQUEST, format!("bad request body: {e}")))
}

/// Non-secret defaults the front prefills the unlock form with.
pub fn defaults() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "nativeWaku": crate::engine::NATIVE_WAKU,
        "waku": {
            "clusterId": railgun_broadcaster::wire::CLUSTER_ID,
            "shardId": railgun_broadcaster::wire::SHARD_ID,
            "bootstrapPeers": railgun_broadcaster::wire::FLEET_WSS_PEERS,
        },
        "trustedFeeSigners": railgun_broadcaster::RAILWAY_TRUSTED_FEE_SIGNERS,
        "trustedFeeSignersSource": "Railway wallet remote configuration, read 2026-09-20",
    })
}

fn status(ctx: &Ctx) -> Result<Value, ApiError> {
    let snapshot = ctx.shared.status.read().map_err(|_| poisoned())?.clone();
    let active = ctx.shared.jobs.lock().ok().and_then(|j| j.active());
    let legacy = ctx
        .shared
        .legacy
        .read()
        .map(|l| l.clone())
        .unwrap_or_default();
    Ok(json!({ "status": snapshot, "activeJob": active, "legacy": legacy }))
}

async fn unlock(ctx: &Ctx, body: Option<Value>) -> Result<Value, ApiError> {
    let params: UnlockParams = parse(body)?;
    let (tx, rx) = oneshot::channel();
    ctx.engine
        .send(Command::Unlock(Box::new(params), tx))
        .await
        .map_err(|_| gone())?;
    match rx.await {
        Ok(Ok(())) => Ok(json!({ "ok": true })),
        Ok(Err(e)) => Err(ApiError::new(BAD_REQUEST, e)),
        Err(_) => Err(gone()),
    }
}

async fn lock(ctx: &Ctx) -> Result<Value, ApiError> {
    let (tx, rx) = oneshot::channel();
    ctx.engine.send(Command::Lock(tx)).await.map_err(|_| gone())?;
    rx.await.map_err(|_| gone())?;
    Ok(json!({ "ok": true }))
}

async fn empty_cache(ctx: &Ctx) -> Result<Value, ApiError> {
    // Queued behind a running job like any command: never deletes under a sync or a proof.
    let (tx, rx) = oneshot::channel();
    ctx.engine
        .send(Command::EmptyCache(tx))
        .await
        .map_err(|_| gone())?;
    match rx.await {
        Ok(Ok(removed)) => Ok(json!({ "ok": true, "removed": removed })),
        Ok(Err(e)) => Err(ApiError::new(CONFLICT, e)),
        Err(_) => Err(gone()),
    }
}

async fn op(ctx: &Ctx, body: Option<Value>) -> Result<Value, ApiError> {
    let op: Op = parse(body)?;
    let unlocked = ctx
        .shared
        .status
        .read()
        .map(|s| s.unlocked)
        .unwrap_or(false);
    if !unlocked {
        return Err(ApiError::new(CONFLICT, "wallet is locked"));
    }
    let id = {
        let mut jobs = ctx.shared.jobs.lock().map_err(|_| poisoned())?;
        jobs.create(op.kind())
    };
    ctx.engine
        .send(Command::Job { id, op })
        .await
        .map_err(|_| gone())?;
    Ok(json!({ "jobId": id }))
}

fn jobs(ctx: &Ctx) -> Result<Value, ApiError> {
    let jobs = ctx.shared.jobs.lock().map_err(|_| poisoned())?;
    Ok(json!({ "jobs": jobs.all() }))
}

fn job(ctx: &Ctx, id: u64) -> Result<Value, ApiError> {
    match ctx.shared.jobs.lock().ok().and_then(|j| j.get(id)) {
        Some(job) => Ok(json!(job)),
        None => Err(ApiError::new(NOT_FOUND, "unknown job")),
    }
}

fn logs(ctx: &Ctx, body: Option<Value>) -> Result<Value, ApiError> {
    // GET query on the daemon, JSON object over IPC: the shim turns `?since=3` into `{"since":"3"}`.
    let since = match body.as_ref().and_then(|b| b.get("since")) {
        None | Some(Value::Null) => 0,
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(_) => 0,
    };
    let logs = ctx.shared.logs.lock().map_err(|_| poisoned())?;
    Ok(json!({ "lines": logs.since(since) }))
}

/// Round trip with the Waku node of the front: takes what it received, returns what it must
/// publish. Payloads are opaque here, fee messages are authenticated further down.
fn waku_exchange(ctx: &Ctx, body: Option<Value>) -> Result<Value, ApiError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use railgun_broadcaster::{PublishAck, RemoteStatus, transport::WakuMessage};

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ExchangeMessage {
        content_topic: String,
        /// base64
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
    struct ExchangeBody {
        connected: bool,
        #[serde(default)]
        peers: usize,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        messages: Vec<ExchangeMessage>,
        /// Outcome of the publishes handed out at earlier exchanges.
        #[serde(default)]
        acks: Vec<ExchangeAck>,
    }

    let body: ExchangeBody = parse(body)?;
    let received = body
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
    let publish: Vec<Value> = ctx
        .shared
        .bridge
        .exchange(
            RemoteStatus {
                connected: body.connected,
                peers: body.peers,
                detail: body.detail,
            },
            received,
            body.acks
                .into_iter()
                .map(|a| PublishAck {
                    id: a.id,
                    peers: a.peers,
                    error: a.error,
                })
                .collect(),
        )
        .into_iter()
        .map(|o| {
            json!({ "id": o.id, "contentTopic": o.content_topic, "payload": STANDARD.encode(&o.payload) })
        })
        .collect();
    Ok(json!({ "publish": publish }))
}

/// Everything a host needs to start the engine, shared by the daemon and the app.
pub struct Boot {
    pub shared: SharedRef,
    pub engine: mpsc::Sender<Command>,
}

/// Installs the tracing subscriber, spawns the engine thread and returns the IPC context.
///
/// `io` is the handle of a multi-thread runtime: Waku I/O runs there because the engine thread is
/// blocked while a proof is computed (ADR-010).
pub fn boot(data_dir: std::path::PathBuf, io: tokio::runtime::Handle) -> anyhow::Result<Boot> {
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

    let shared = Arc::new(crate::shared::Shared::default());
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,railgun=info,railgun_wallet=info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(crate::shared::FrontLogLayer(shared.clone()))
        .try_init();

    std::fs::create_dir_all(&data_dir)?;
    let engine = crate::engine::spawn(shared.clone(), data_dir, io);
    Ok(Boot { shared, engine })
}

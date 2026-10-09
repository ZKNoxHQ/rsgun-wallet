//! HTTP API consumed by the embedded front. A thin axum wrapper over `ipc::dispatch`, which is
//! also what the Android app calls over Tauri IPC (ADR-024).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    engine::Command,
    ipc::{self, ApiError, Ctx},
    shared::SharedRef,
};

const INDEX_HTML: &str = include_str!("../static/index.html");
/// js-waku light node and its glue, built from `waku-bridge/` (see its package.json).
const WAKU_BUNDLE_JS: &str = include_str!("../static/waku-bundle.js");

#[derive(Clone)]
pub struct AppState {
    pub shared: SharedRef,
    pub engine: mpsc::Sender<Command>,
    /// Origins the front may be served from (loopback only). Anything else is refused.
    pub origins: Arc<Vec<String>>,
}

impl AppState {
    fn ctx(&self) -> Ctx {
        Ctx {
            shared: self.shared.clone(),
            engine: self.engine.clone(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/waku-bundle.js", get(waku_bundle))
        .route("/api/waku/exchange", post(call))
        .route("/api/defaults", get(call))
        .route("/api/status", get(call))
        .route("/api/unlock", post(call))
        .route("/api/lock", post(call))
        .route("/api/empty-cache", post(call))
        .route("/api/op", post(call))
        .route("/api/jobs", get(call))
        .route("/api/jobs/{id}", get(job))
        .route("/api/logs", get(logs))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            same_origin,
        ))
        .with_state(state)
}

/// The daemon holds keys and listens on localhost: refuse cross-origin callers (a web page in
/// the same browser could otherwise POST to it) and DNS-rebinding style Host headers.
async fn same_origin(
    State(state): State<AppState>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    let headers = req.headers();
    let host_ok = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| state.origins.iter().any(|o| o.ends_with(&format!("//{h}"))));
    let origin_ok = match headers.get(header::ORIGIN) {
        None => true,
        Some(o) => o
            .to_str()
            .is_ok_and(|o| state.origins.iter().any(|allowed| allowed == o)),
    };
    if !host_ok || !origin_ok {
        return err(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    next.run(req).await
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

fn answer(result: Result<Value, ApiError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => {
            let code = StatusCode::from_u16(e.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            err(code, e.message)
        }
    }
}

/// Every endpoint whose path is its own route key: the body (if any) goes straight to dispatch.
async fn call(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let body = match axum::body::to_bytes(req.into_body(), 2 * 1024 * 1024).await {
        Ok(bytes) if bytes.is_empty() => None,
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => Some(v),
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("bad request body: {e}")),
        },
        Err(_) => return err(StatusCode::BAD_REQUEST, "body too large"),
    };
    answer(ipc::dispatch(&state.ctx(), &path, body).await)
}

async fn job(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    answer(ipc::dispatch(&state.ctx(), &format!("/api/jobs/{id}"), None).await)
}

#[derive(serde::Deserialize)]
struct LogsQuery {
    #[serde(default)]
    since: u64,
}

async fn logs(State(state): State<AppState>, Query(q): Query<LogsQuery>) -> Response {
    let body = json!({ "since": q.since });
    answer(ipc::dispatch(&state.ctx(), "/api/logs", Some(body)).await)
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    // wss: the Railgun Waku fleet, dialled by the js-waku node of this page.
                    "default-src 'none'; script-src 'self' 'unsafe-inline'; \
                     style-src 'unsafe-inline'; \
                     connect-src 'self' wss://*.rootedinprivacy.com:8000; \
                     base-uri 'none'; form-action 'none'",
                ),
            ),
        ],
        Html(INDEX_HTML),
    )
}

async fn waku_bundle() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/javascript; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        WAKU_BUNDLE_JS,
    )
}

//! Dr Rail as a static web page. The session core of `railgun-viewer` (keys, signer resolution,
//! sync, history, POI probes, network health, Waku link) is compiled to wasm from the very same
//! source files and runs in a Web Worker; the page is the viewer's own front, whose `/api/…`
//! calls are routed to [`api`] by `web/shim.js` instead of a local daemon.
#![cfg(target_arch = "wasm32")]
#![allow(dead_code)]

#[path = "../../railgun-wallet/src/keys.rs"]
pub(crate) mod wallet_keys;

#[path = "../../railgun-viewer/src/chain.rs"]
mod chain;
#[path = "../../railgun-viewer/src/health.rs"]
mod health;
#[path = "../../railgun-viewer/src/history.rs"]
mod history;
#[path = "../../railgun-viewer/src/keys.rs"]
mod keys;
#[path = "../../railgun-viewer/src/session.rs"]
mod session;
#[path = "../../railgun-viewer/src/shared.rs"]
mod shared;
#[path = "../../railgun-viewer/src/signer.rs"]
mod signer;
#[path = "../../railgun-viewer/src/waku_link.rs"]
mod waku_link;

mod web;

pub use web::api;

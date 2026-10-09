//! The railgun-wallet daemon as a static web page. The wallet engine is compiled to wasm and runs
//! in a Web Worker; the page is the daemon's own front (`railgun-wallet/static/index.html`), whose
//! `/api/…` calls are routed to [`api`] by `web/shim.js` instead of the local daemon. Same paths,
//! same JSON bodies (`railgun-wallet/src/ipc.rs`), so the front runs unchanged.
#![cfg(target_arch = "wasm32")]
#![allow(dead_code)]

// Reused verbatim from the daemon so the front sees identical JSON shapes.
#[path = "../../railgun-wallet/src/keys.rs"]
pub(crate) mod keys;
#[path = "../../railgun-wallet/src/shared.rs"]
pub(crate) mod shared;

mod ble_bridge;
mod web;

pub use ble_bridge::ble_notify;
pub use web::api;

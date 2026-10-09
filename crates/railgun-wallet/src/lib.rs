//! ZKNOX Railgun wallet core: engine, keys, storage and the transport-free API dispatch.
//!
//! Two hosts use it. `src/main.rs` is the desktop daemon (axum on loopback, `api` feature).
//! `crates/railgun-wallet-android` is the Tauri app, which calls `ipc::dispatch` directly and
//! never opens a socket (ADR-025).

pub mod db;
pub mod engine;
pub mod ipc;
pub mod keys;
pub mod shared;

#[cfg(feature = "http")]
pub mod api;

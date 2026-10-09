//! ZKNOX Railgun viewer: read-only history and note lineage, from a mnemonic or a viewing key.
//!
//! Companion of `railgun-wallet`: same SDK, same key derivation, same on-disk database format,
//! but it never holds a spending key unless a mnemonic is given, and it never spends.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use tracing::info;

mod api;
mod chain;
mod engine;
mod health;
mod history;
mod keys;
mod session;
mod shared;
mod signer;
mod waku_link;

/// The wallet's key derivation, under the name the shared modules use (`crate::wallet_keys`); the
/// web build includes the same file under that name.
pub(crate) use railgun_wallet::keys as wallet_keys;

struct Args {
    port: u16,
    data_dir: PathBuf,
}

fn parse_args() -> Result<Args> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut port = 8790u16;
    let mut data_dir = home.join(".railgun-viewer");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().context("--port needs a value")?.parse()?,
            "--data-dir" => data_dir = args.next().context("--data-dir needs a value")?.into(),
            "-h" | "--help" => {
                println!(
                    "railgun-viewer [--port 8790] [--data-dir ~/.railgun-viewer]\n\n\
                     Pass --data-dir ~/.railgun-wallet to reuse the wallet's synced database \
                     (same address, same layout); do not run both at the same time on it."
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(Args { port, data_dir })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = parse_args()?;
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating {}", args.data_dir.display()))?;

    let shared = shared::new_shared();
    let engine = engine::spawn(args.data_dir.clone(), shared.clone());

    // Loopback only: the API accepts key material.
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let app = api::router(api::AppState {
        engine,
        shared,
        version: env!("CARGO_PKG_VERSION"),
        data_dir: Arc::new(args.data_dir.display().to_string()),
        bridge: Arc::new(railgun_broadcaster::BrowserBridge::new()),
        bridge_clients: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        native_clients: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    });
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    info!("railgun-viewer listening on http://{addr} (data dir: {})", args.data_dir.display());
    axum::serve(listener, app).await?;
    Ok(())
}

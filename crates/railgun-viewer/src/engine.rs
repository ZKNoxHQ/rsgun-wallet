//! Engine actor. One thread, one current-thread runtime, owns the `RailgunProvider` (its futures
//! are not `Send`). Commands arrive over a channel; results land in `Shared`. The session logic
//! itself lives in `session.rs`, shared with the web build.

use std::{path::PathBuf, sync::Arc};

use alloy::{
    network::Ethereum,
    providers::{Provider, ProviderBuilder},
};
use anyhow::{Context, Result, anyhow, bail};
use railgun::{builder::RailgunBuilder, chain_config::ChainConfig};
use railgun_wallet::db;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

pub use crate::session::UnlockParams;
use crate::{
    keys,
    session::{self, CacheStore, Caches, Session},
    shared::{SharedRef, log, now_ms},
};

pub enum Command {
    Unlock(Box<UnlockParams>, oneshot::Sender<Result<()>>),
    Sync,
    Refresh,
    Lock(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct Engine {
    tx: mpsc::UnboundedSender<Command>,
}

impl Engine {
    pub async fn unlock(&self, p: UnlockParams) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Unlock(Box::new(p), tx))
            .map_err(|_| anyhow!("engine thread is gone"))?;
        rx.await.map_err(|_| anyhow!("engine dropped the request"))?
    }

    pub fn sync(&self) -> Result<()> {
        self.tx.send(Command::Sync).map_err(|_| anyhow!("engine thread is gone"))
    }

    pub fn refresh(&self) -> Result<()> {
        self.tx.send(Command::Refresh).map_err(|_| anyhow!("engine thread is gone"))
    }

    pub async fn lock(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Command::Lock(tx)).map_err(|_| anyhow!("engine thread is gone"))?;
        rx.await.map_err(|_| anyhow!("engine dropped the request"))
    }
}

pub fn spawn(base_dir: PathBuf, shared: SharedRef) -> Engine {
    let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
    std::thread::Builder::new()
        .name("railgun-viewer-engine".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("engine runtime");
            rt.block_on(async move {
                let mut actor = Actor {
                    base_dir,
                    shared,
                    session: None,
                };
                while let Some(cmd) = rx.recv().await {
                    actor.handle(cmd).await;
                }
            });
        })
        .expect("engine thread");
    Engine { tx }
}

/// `<data-dir>/<chain>/<sha256(address)[..8]>/viewer-cache.json`.
struct FileCache {
    path: PathBuf,
}

impl CacheStore for FileCache {
    fn save(&self, value: Value) {
        let res = self
            .path
            .parent()
            .map(std::fs::create_dir_all)
            .unwrap_or(Ok(()))
            .and_then(|_| std::fs::write(&self.path, serde_json::to_vec(&value).unwrap_or_default()));
        if let Err(e) = res {
            tracing::warn!("cannot write viewer cache: {e}");
        }
    }
}

fn load_caches(path: &PathBuf) -> Caches {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|v| Caches::from_value(&v))
        .unwrap_or_default()
}

struct Actor {
    base_dir: PathBuf,
    shared: SharedRef,
    session: Option<Session>,
}

impl Actor {
    async fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Unlock(p, reply) => {
                // Reply at once: the discovery scan (view-only) can take minutes and the HTTP
                // request must not carry it. Progress goes to `Shared` (stage + log).
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
                let res = self.unlock(*p).await;
                match res {
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
        error!("{msg}");
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
        let rpc_chain = provider.get_chain_id().await.context("eth_chainId")?;
        if rpc_chain != chain.id {
            bail!("RPC is on chain {rpc_chain}, expected {}", chain.id);
        }

        let (signer, scheme) =
            session::make_signer(resolved, keys::non_empty(&p.address), &chain, &provider, &self.shared).await?;
        let address = signer.address();

        // Same layout as railgun-wallet: <data-dir>/<chain>/<sha256(address)[..8]>/db-v3.redb.
        let tag = hex::encode(&Sha256::digest(address.to_string().as_bytes())[..8]);
        let data_dir = self.base_dir.join(chain.id.to_string()).join(tag);
        let db = db::open(&data_dir)?;

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
        let cache_path = data_dir.join("viewer-cache.json");
        let caches = load_caches(&cache_path);
        info!(%address, chain = chain.id, mode, scheme, poi = p.poi, rpc = %rpc_url, "viewer unlocked");
        log(
            &self.shared,
            format!("✓ {mode} · {address} · chain {} · POI {}", chain.id, if p.poi { "on" } else { "off" }),
        );
        self.session = Some(Session::new(
            chain,
            provider,
            railgun,
            signer,
            mode,
            Box::new(FileCache { path: cache_path }),
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
        let res = session.railgun.sync().await;
        match res {
            Ok(()) => log(
                &self.shared,
                format!("✓ synced to block {}", session.railgun.synced_block()),
            ),
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

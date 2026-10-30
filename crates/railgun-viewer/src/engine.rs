//! Engine actor. One thread, one current-thread runtime, owns the `RailgunProvider` (its futures
//! are not `Send`). Commands arrive over a channel; results land in `Shared`. The session logic
//! itself lives in `session.rs`, shared with the web build.

use std::{
    path::PathBuf,
};

use alloy::{
    network::Ethereum,
    providers::{Provider, ProviderBuilder},
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use railgun::{RailgunState, builder::RailgunBuilder};
use railgun_wallet::db;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

pub use crate::session::UnlockParams;
use crate::{
    keys,
    session::{self, Caches, Session, Store},
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

/// The account's directory, `<data-dir>/<chain>/<sha256(address)[..8]>/`, the wallet's layout:
/// the SDK state in `railgun-state.bin` (the wallet's snapshot store, `railgun_wallet::db`, so
/// the two read each other's state) and the viewer cache in `viewer-cache.json`. Earlier versions
/// kept the SDK state in a redb database (`db-v3.redb`) in the same directory; that file is not
/// read any more and is left alone.
struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    /// Creates the directory (readable by the owner only: it holds decrypted notes) and restores
    /// the saved SDK state, a fresh one when there is none yet. A snapshot that cannot be read is
    /// an error naming the file rather than a silent resync from scratch.
    fn open(dir: PathBuf) -> Result<(Self, RailgunState)> {
        let state = db::open(&dir)?;
        Ok((Self { dir }, state))
    }

    fn state_path(&self) -> PathBuf {
        db::snapshot_path(&self.dir)
    }

    fn cache_path(&self) -> PathBuf {
        self.dir.join("viewer-cache.json")
    }

    fn load_caches(&self) -> Caches {
        std::fs::read(self.cache_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .map(|v| Caches::from_value(&v))
            .unwrap_or_default()
    }
}

#[async_trait(?Send)]
impl Store for FileStore {
    fn save_cache(&self, value: Value) {
        let res = std::fs::create_dir_all(&self.dir)
            .and_then(|()| std::fs::write(self.cache_path(), serde_json::to_vec(&value).unwrap_or_default()));
        if let Err(e) = res {
            tracing::warn!("cannot write viewer cache: {e}");
        }
    }

    async fn save_state(&self, bytes: Vec<u8>) -> Result<()> {
        db::save_bytes(&self.dir, &bytes)
    }
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
        let chain = railgun_chains::from_chain_id(p.chain_id)
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
            session::make_signer(resolved, keys::non_empty(&p.address), &chain, &self.shared).await?;
        let address = signer.address();

        // Same layout as railgun-wallet: <data-dir>/<chain>/<sha256(address)[..8]>/.
        let tag = hex::encode(&Sha256::digest(address.to_string().as_bytes())[..8]);
        let data_dir = self.base_dir.join(chain.id.to_string()).join(tag);
        let (store, state) = FileStore::open(data_dir)?;

        let mut builder = RailgunBuilder::new(chain.clone(), provider.clone()).with_state(state.clone());
        if p.poi {
            builder = builder.with_poi();
        }
        let mut railgun = builder.build().await.context("building railgun provider")?;
        railgun.register(signer.clone()).await?;

        let poi_endpoint = chain.poi_endpoint.clone();
        let caches = store.load_caches();
        info!(%address, chain = chain.id, mode, scheme, poi = p.poi, rpc = %rpc_url, "viewer unlocked");
        log(
            &self.shared,
            format!("✓ {mode} · {address} · chain {} · POI {}", chain.id, if p.poi { "on" } else { "off" }),
        );
        let session = Session::new(chain, provider, railgun, state, signer, mode, Box::new(store), caches);
        // Building and registering can already change the state (account registration, tree
        // setup): save it so that the file exists from the first unlock.
        session.persist_state().await?;
        self.session = Some(session);
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
        let res = session::sync_with_progress(session, &self.shared).await;
        let synced = session.railgun.synced_block();
        // Whatever the outcome, the state has moved: save it (the progress up to a failure is
        // kept too), and say so when it cannot be saved, since the next unlock would then start
        // again from the last saved block.
        let saved = session.persist_state().await;
        if let Ok(mut s) = self.shared.lock() {
            s.stage = None;
            s.stage_detail = None;
        }
        match res {
            Ok(()) => log(&self.shared, format!("✓ synced to block {synced}")),
            Err(e) => self.set_error(format!("sync failed: {e:#}")),
        }
        if let Err(e) = saved {
            self.set_error(format!("{e:#}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn state_file_round_trips_and_refuses_garbage() {
        let dir = std::env::temp_dir().join(format!("railgun-viewer-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let (store, state) = FileStore::open(dir.clone()).unwrap();
        assert!(!store.state_path().exists(), "no file before the first save");
        let bytes = session::state_bytes(&state).unwrap();
        store.save_state(bytes.clone()).await.unwrap();
        assert_eq!(std::fs::read(store.state_path()).unwrap(), bytes);
        assert!(!dir.join("railgun-state.bin.tmp").exists(), "the temporary file is renamed away");

        let (_, restored) = FileStore::open(dir.clone()).unwrap();
        assert_eq!(session::state_bytes(&restored).unwrap(), bytes);

        std::fs::write(store.state_path(), [0xff; 7]).unwrap();
        let Err(err) = FileStore::open(dir.clone()) else {
            panic!("garbage opened as a Railgun state");
        };
        assert!(err.to_string().contains("railgun-state.bin"), "{err:#}");

        std::fs::remove_dir_all(&dir).ok();
    }
}

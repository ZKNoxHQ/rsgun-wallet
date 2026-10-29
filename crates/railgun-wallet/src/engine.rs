//! Wallet engine.
//!
//! A single actor, running on its own thread with a current-thread tokio runtime, owns the
//! `RailgunProvider`. It needs `&mut self` for nearly everything and its futures are not
//! guaranteed to be `Send`, so commands are serialised through an mpsc channel instead of being
//! run inside axum handlers.

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use alloy::{
    network::Ethereum,
    primitives::{Address, B256, U256, utils::format_units, utils::parse_units},
    rpc::types::TransactionRequest,
    providers::{DynProvider, Provider, ProviderBuilder},
    signers::local::PrivateKeySigner as EoaSigner,
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result, anyhow, bail};
use railgun::{
    RailgunState,
    account::{
        address::{PqRailgunAddress, RailgunAddress},
        chain::ChainId as RgChainId,
        signer::{PrivateKeySigner as RgSigner, RailgunSigner},
    },
    builder::RailgunBuilder,
    caip::AssetId,
    chain_config::ChainConfig,
    crypto::keys::SpendingPublicKey,
    provider::RailgunProvider,
    transact::{AmountIntents, NoteEncryption, Spend},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use userop_kit::{
    bundler::{Bundler, pimlico::PimlicoBundler},
    smart_account::simple_smart_account::{Call, SimpleSmartAccount},
};

use railgun_broadcaster::{
    BroadcastRequest, BroadcasterClient, ClientError, FeeQuote, NoQuote, NwakuRest, WakuTransport,
    PAR_RATE_WRAPPED_BASE_TOKEN, RelayAction, fees::now_ms as quote_now_ms, token_fee,
};

use crate::{
    db,
    keys::{self, Derivation},
    shared::{JobState, LegacyStatus, SharedRef, StatusSnapshot, TransportInfo, now_ms},
};

sol! {
    #[sol(rpc)]
    contract ERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
    }

    contract WETH9 {
        function withdraw(uint256 wad) external;
    }
}

/// Public endpoints used when the front leaves the field empty. Rate limited, fine for testing.
fn default_rpc_url(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("https://ethereum-rpc.publicnode.com"),
        11_155_111 => Some("https://ethereum-sepolia-rpc.publicnode.com"),
        _ => None,
    }
}

fn default_bundler_url(chain_id: u64) -> String {
    format!("https://public.pimlico.io/v2/{chain_id}/rpc")
}

/// Cached *public* material for a Ledger account, so a re-open can skip the spending-pubkey
/// prompt. No secret is stored here: the viewing key is re-exported from the device each
/// unlock and its public key validates these values (see `LedgerSigner::connect_cached`).
#[derive(Serialize, Deserialize)]
struct LedgerCache {
    spending_pubkey: SpendingPublicKey,
    address: String,
}

fn ledger_cache_path(base_dir: &Path, chain_id: u64, account: u32) -> PathBuf {
    base_dir
        .join(chain_id.to_string())
        .join(format!("ledger-{account}.json"))
}

fn read_ledger_cache(path: &Path) -> Option<(SpendingPublicKey, RailgunAddress)> {
    let bytes = std::fs::read(path).ok()?;
    let cache: LedgerCache = serde_json::from_slice(&bytes).ok()?;
    let address = RailgunAddress::from_str(&cache.address).ok()?;
    Some((cache.spending_pubkey, address))
}

fn write_ledger_cache(path: &Path, spending_pubkey: SpendingPublicKey, address: &RailgunAddress) {
    let cache = LedgerCache {
        spending_pubkey,
        address: address.to_string(),
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&cache) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Public data, but keep it owner-only alongside the wallet's other files.
    if std::fs::write(path, &bytes).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// A broadcaster of the reference implementation takes its gas cost plus a margin of 10 to
/// 30%. Half as much again leaves room for that and refuses anything predatory.
const DEFAULT_MAX_FEE_RATE: &str = "1.5";

/// The draw is among offers at most this much above the cheapest, as in the reference client.
const BROADCASTER_DRAW_PERCENT: u32 = 10;

/// A broadcaster that did not answer is left out of the draw for this long.
const SILENT_BROADCASTER_PENALTY: Duration = Duration::from_secs(600);

const DEFAULT_WAKU_URL: &str = "http://127.0.0.1:8645";

/// Whether this build carries the native Waku light node (feature `native-waku`).
pub const NATIVE_WAKU: bool = cfg!(feature = "native-waku");

/// Waku light node in this process, dialling the Railgun fleet itself. Starts on the first call
/// of the fee monitor, on the I/O runtime, and stops when the session is replaced.
#[cfg(feature = "native-waku")]
fn native_transport(chain_id: u64) -> Arc<dyn WakuTransport> {
    Arc::new(railgun_broadcaster::LightNodeTransport::for_chain(chain_id))
}

#[cfg(not(feature = "native-waku"))]
fn native_transport(_chain_id: u64) -> Arc<dyn WakuTransport> {
    unreachable!("\"native\" is mapped to \"browser\" without the native-waku feature")
}

/// Gas assumed for the first fee guess, before the dummy-proof estimate replaces it.
const FIRST_GUESS_GAS: u64 = 700_000;

/// Keeps the fee cache current and publishes the state of the broadcaster network for the front.
async fn fee_monitor(
    client: Arc<BroadcasterClient>,
    shared: SharedRef,
    mode: &'static str,
    waku_url: String,
) {
    let mut subscribed = false;
    loop {
        let mut status = LegacyStatus {
            mode: Some(mode.to_string()),
            waku_url: (mode == "nwaku").then(|| waku_url.clone()),
            updated_at: now_ms(),
            ..Default::default()
        };
        let result = async {
            if !subscribed {
                client.subscribe().await?;
            }
            client.pump().await
        }
        .await;
        match result {
            Ok(_) => {
                subscribed = true;
                status.reachable = true;
                status.peers = client.peer_count().await;
                status.trusted_signers = client.trusted_signer_count();
                status.authorized_fees = client
                    .authorized_fees()
                    .into_iter()
                    .map(|(token, rate)| json!({ "token": token, "feePerUnitGas": rate.to_string() }))
                    .collect();
                status.quotes = client
                    .all_quotes()
                    .into_iter()
                    .filter_map(|q| serde_json::to_value(q).ok())
                    .collect();
            }
            Err(e) => {
                subscribed = false;
                status.error = Some(e.to_string());
            }
        }
        if let Ok(mut l) = shared.legacy.write() {
            *l = status;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Seconds to wait after an on-chain inclusion before re-syncing, so that the subsquid indexer
/// has a chance to see the new commitments.
const POST_TX_SYNC_DELAY: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// ERC-4337 UserOperation through the kohaku privacy paymaster.
    Erc4337,
    /// Railgun community broadcasters (Waku). Not implemented yet.
    Legacy,
    /// Self-broadcast from the public EOA. No sender privacy, debug only.
    Direct,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockParams {
    pub mnemonic: Option<String>,
    pub spending_key: Option<String>,
    pub viewing_key: Option<String>,
    #[serde(default)]
    pub derivation: Derivation,
    #[serde(default)]
    pub index: u32,
    pub chain_id: u64,
    /// Sign with a Ledger device running the ZKNOX Railgun app: the spending key never
    /// leaves the device, the viewing key is exported once at unlock. `index` selects the
    /// on-device account; mnemonic and raw-key fields are ignored.
    #[serde(default)]
    pub ledger: bool,
    /// Transport for the Ledger: "usb" (default) or "ble" (Flex/Stax/Nano X).
    pub ledger_transport: Option<String>,
    /// Empty or absent: public default for the chain.
    pub rpc_url: Option<String>,
    /// Public EOA used for shielding (and for the `direct` transport).
    pub eoa_key: Option<String>,
    #[serde(default)]
    pub poi: bool,
    pub bundler_url: Option<String>,
    /// REST root of a local nwaku node on the Railgun shard, for the legacy transport.
    pub waku_url: Option<String>,
    /// "native" (default): Waku light node inside the daemon. "browser": js-waku in the wallet
    /// tab. "nwaku": local node at `waku_url`.
    pub waku_mode: Option<String>,
    /// 0zk addresses of trusted fee signers, separated by commas, spaces or new lines. Offers of
    /// other broadcasters are then capped to a band around the signers' rates.
    pub trusted_fee_signers: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Op {
    Sync,
    #[serde(rename_all = "camelCase")]
    Shield { asset: String, amount: String },
    #[serde(rename_all = "camelCase")]
    Transfer {
        to: String,
        asset: String,
        amount: String,
        #[serde(default)]
        memo: String,
        transport: Transport,
        #[serde(default, flatten)]
        fee_limits: FeeLimits,
    },
    #[serde(rename_all = "camelCase")]
    Unshield {
        to: String,
        asset: String,
        amount: String,
        /// Unwrap the wrapped base token and deliver native currency to `to`.
        #[serde(default)]
        native: bool,
        transport: Transport,
        #[serde(default, flatten)]
        fee_limits: FeeLimits,
    },
}

/// Ceilings on what a broadcaster may charge (legacy transport). They need no trusted party:
/// the fee token is the wrapped base token, so the honest rate is the gas cost plus a margin.
#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FeeLimits {
    /// Highest accepted rate, as a multiple of the gas cost ("1.5" = 50% over). Defaults to
    /// [`DEFAULT_MAX_FEE_RATE`] when absent or empty.
    pub max_fee_rate: Option<String>,
    /// Highest accepted fee for the whole transaction, in wrapped base token ("0.002").
    pub max_fee: Option<String>,
    /// 4337: prove once, with gas limits fixed beforehand from the bundler's estimate of the
    /// dummy-proof operation, instead of re-proving until the fee converges. One spending
    /// signature per operation, which is what a hardware or threshold signer needs.
    #[serde(default)]
    pub single_proof: bool,
    /// Encrypt the change notes with the post-quantum scheme, to this wallet's own ML-KEM key.
    /// Wallets without that key (legacy ones on the same seed) will not see the change.
    #[serde(default)]
    pub hybrid_change: bool,
    /// Legacy: 0zk address of the broadcaster to use, instead of a draw among the cheapest.
    pub broadcaster: Option<String>,
    /// With `single_proof`: fail rather than fall back to the iterative path when the limits
    /// cannot be simulated. For signers where several signatures are not an option.
    #[serde(default)]
    pub single_proof_strict: bool,
}

impl Op {
    pub fn kind(&self) -> &'static str {
        match self {
            Op::Sync => "sync",
            Op::Shield { .. } => "shield",
            Op::Transfer { .. } => "transfer",
            Op::Unshield { .. } => "unshield",
        }
    }
}

pub enum Command {
    Unlock(Box<UnlockParams>, oneshot::Sender<Result<(), String>>),
    Lock(oneshot::Sender<()>),
    /// Deletes the state snapshot of the open wallet and locks it.
    EmptyCache(oneshot::Sender<Result<String, String>>),
    Job { id: u64, op: Op },
}

#[derive(Clone)]
struct TokenMeta {
    symbol: String,
    decimals: u8,
}

struct Session {
    chain: ChainConfig,
    provider: DynProvider,
    eoa: Option<EoaSigner>,
    eoa_source: Option<String>,
    signer: Arc<dyn RailgunSigner>,
    railgun: RailgunProvider,
    /// The state `railgun` works on, kept to write snapshots of it to `data_dir`.
    state: RailgunState,
    derivation: &'static str,
    bundler_url: String,
    data_dir: PathBuf,
    tokens: HashMap<Address, TokenMeta>,
    broadcaster: Arc<BroadcasterClient>,
    /// The tab's Waku link when that is the transport, to read what happened to publishes.
    bridge: Option<Arc<railgun_broadcaster::BrowserBridge>>,
    /// Broadcasters that did not answer, with the time of the failure. Skipped for a while.
    silent_broadcasters: Vec<(String, std::time::Instant)>,
    /// Limits of the operation being run, set by `Engine::run`.
    fee_limits: FeeLimits,
    /// Multi-thread runtime of the HTTP server. Waku I/O runs there: this thread's runtime is
    /// blocked while a proof is computed.
    io: tokio::runtime::Handle,
    fee_monitor: tokio::task::AbortHandle,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.fee_monitor.abort();
    }
}

pub struct Engine {
    shared: SharedRef,
    base_dir: PathBuf,
    io: tokio::runtime::Handle,
    session: Option<Session>,
}

/// Spawns the engine thread and returns the command channel.
pub fn spawn(
    shared: SharedRef,
    base_dir: PathBuf,
    io: tokio::runtime::Handle,
) -> mpsc::Sender<Command> {
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    std::thread::Builder::new()
        .name("railgun-engine".into())
        // Groth16 proving recurses deep enough to be uncomfortable on the default 2 MiB.
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("engine runtime");
            rt.block_on(async move {
                let mut engine = Engine {
                    shared,
                    base_dir,
                    io,
                    session: None,
                };
                while let Some(cmd) = rx.recv().await {
                    engine.handle(cmd).await;
                }
            });
        })
        .expect("spawn engine thread");
    tx
}

struct JobCtx<'a> {
    shared: &'a SharedRef,
    id: u64,
}

impl JobCtx<'_> {
    fn step(&self, msg: impl Into<String>) {
        let msg = msg.into();
        info!(job = self.id, "{msg}");
        if let Ok(mut jobs) = self.shared.jobs.lock() {
            jobs.update(self.id, |j| j.steps.push(msg));
        }
    }
}

impl Engine {
    async fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Unlock(params, reply) => {
                let res = self.unlock(*params).await.map_err(|e| format!("{e:#}"));
                if let Err(e) = &res {
                    error!("unlock failed: {e}");
                }
                let _ = reply.send(res);
            }
            Command::EmptyCache(reply) => {
                let res = self.empty_cache().map_err(|e| format!("{e:#}"));
                let _ = reply.send(res);
            }
            Command::Lock(reply) => {
                self.session = None;
                if let Ok(mut s) = self.shared.status.write() {
                    *s = StatusSnapshot {
                        updated_at: now_ms(),
                        ..Default::default()
                    };
                }
                if let Ok(mut l) = self.shared.legacy.write() {
                    *l = LegacyStatus::default();
                }
                info!("wallet locked");
                let _ = reply.send(());
            }
            Command::Job { id, op } => {
                self.set_job(id, |j| j.state = JobState::Running);
                let shared = self.shared.clone();
                let ctx = JobCtx {
                    shared: &shared,
                    id,
                };
                let res = self.run(&ctx, op).await;
                // Whatever the outcome: a snapshot is consistent at any point, and a failed
                // command may still have synced or registered pending POI work worth keeping.
                self.persist_state();
                self.refresh_snapshot().await;
                match res {
                    Ok(value) => self.set_job(id, |j| {
                        j.state = JobState::Done;
                        j.result = Some(value);
                        j.finished_at = Some(now_ms());
                    }),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        error!(job = id, "job failed: {msg}");
                        self.set_job(id, |j| {
                            j.state = JobState::Failed;
                            j.error = Some(msg);
                            j.finished_at = Some(now_ms());
                        });
                    }
                }
            }
        }
    }

    fn set_job(&self, id: u64, f: impl FnOnce(&mut crate::shared::Job)) {
        if let Ok(mut jobs) = self.shared.jobs.lock() {
            jobs.update(id, f);
        }
    }

    /// Writes the session's SDK state to disk. A failure is logged, not fatal: the state is
    /// still in memory and the next command tries again.
    fn persist_state(&self) {
        if let Some(s) = &self.session {
            if let Err(e) = s.persist() {
                error!("saving the wallet state failed: {e:#}");
            }
        }
    }

    /// Removes everything the wallet cached for the open account on this chain: synced
    /// commitments, decrypted notes, txid trees, pending POI entries. Keys are not on disk, so
    /// the wallet is locked and has to be reopened; the first sync is then a full one. Proofs
    /// still owed for mined transactions are rebuilt from chain data by the POI recovery.
    /// `ephemeral_senders.jsonl` is kept: it holds keys that may still control funds.
    fn empty_cache(&mut self) -> Result<String> {
        let session = self
            .session
            .take()
            .ok_or_else(|| anyhow!("open the wallet first: the cache is per account and chain"))?;
        let data_dir = session.data_dir.clone();
        // Drops the in-memory state and stops the fee monitor before touching the files, so
        // nothing writes a snapshot back after the deletion.
        drop(session);
        if let Ok(mut s) = self.shared.status.write() {
            *s = StatusSnapshot {
                updated_at: now_ms(),
                ..Default::default()
            };
        }
        if let Ok(mut l) = self.shared.legacy.write() {
            *l = LegacyStatus::default();
        }

        // The snapshot of this version, then the directories of earlier versions.
        let mut removed = db::delete(&data_dir)?;
        for entry in std::fs::read_dir(&data_dir).with_context(|| format!("reading {}", data_dir.display()))? {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if path.is_dir() && name.starts_with("db") {
                std::fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
                removed.push(name);
            }
        }
        info!("cache emptied: {} ({:?})", data_dir.display(), removed);
        Ok(format!("{} ({})", data_dir.display(), removed.join(", ")))
    }

    async fn unlock(&mut self, p: UnlockParams) -> Result<()> {
        let chain = railgun_chains::from_chain_id(p.chain_id)
            .ok_or_else(|| anyhow!("unsupported chain id {}", p.chain_id))?;

        // Chain-agnostic address, as Railway displays it: the chain field of a 0zk address is
        // purely advisory, and one address per wallet is less confusing than one per chain.
        let (signer, keys, derivation): (Arc<dyn RailgunSigner>, Option<keys::RailgunKeys>, &'static str) =
            if p.ledger {
                let transport = p.ledger_transport.as_deref().unwrap_or("usb");
                // Public material cached from a previous unlock (0zk address + spending
                // pubkey). Lets connect_cached skip the spending-pubkey prompt when the
                // exported viewing key still matches — so the normal case is one prompt.
                let cache_path = ledger_cache_path(&self.base_dir, chain.id, p.index);
                let cached = read_ledger_cache(&cache_path);
                let signer: Arc<dyn RailgunSigner> = match transport {
                    "ble" | "bluetooth" => {
                        let device = railgun_ledger::BleLedger::connect()
                            .await
                            .map_err(|e| anyhow!("Ledger over BLE: {e}"))?;
                        railgun_ledger::LedgerSigner::connect_cached(
                            device, RgChainId::All, p.index, cached,
                        )
                        .await
                        .map_err(|e| anyhow!("Ledger Railgun app: {e}"))?
                    }
                    "usb" => {
                        let device = railgun_ledger::transport::usb::UsbLedger::init()
                            .await
                            .map_err(|e| anyhow!("Ledger: {e} (device plugged in and unlocked?)"))?;
                        railgun_ledger::LedgerSigner::connect_cached(
                            device, RgChainId::All, p.index, cached,
                        )
                        .await
                        .map_err(|e| anyhow!("Ledger Railgun app: {e}"))?
                    }
                    other => bail!("unknown Ledger transport {other:?} (use \"usb\" or \"ble\")"),
                };
                // Refresh the cache with whatever the signer settled on (unchanged on a
                // cache hit, updated when the spending pubkey was re-fetched).
                write_ledger_cache(&cache_path, signer.spending_public_key(), &signer.address());
                (signer, None, "ledger")
            } else {
                let (keys, derivation) = match (&p.mnemonic, &p.spending_key, &p.viewing_key) {
                    (Some(m), _, _) if !m.trim().is_empty() => (
                        keys::derive(m, p.index, p.derivation)?,
                        match p.derivation {
                            Derivation::Railgun => "railgun",
                            Derivation::Kohaku => "kohaku",
                        },
                    ),
                    (_, Some(s), Some(v)) => (keys::from_hex(s, v)?, "raw"),
                    _ => bail!(
                        "provide a mnemonic, both spending and viewing keys, or a Ledger device"
                    ),
                };
                let signer = RgSigner::new_with_kem(
                    keys.spending,
                    keys.viewing,
                    keys.kem.clone(),
                    RgChainId::All,
                );
                (signer, Some(keys), derivation)
            };
        let address = signer.address().to_string();

        // Public account: an explicit key wins; otherwise the Ethereum account of the same
        // phrase, at the same index, as every mnemonic wallet derives it. A Ledger unlock has
        // no phrase: only an explicit key provides one.
        let mnemonic = if p.ledger { &None } else { &p.mnemonic };
        let (eoa, eoa_source) = match (p.eoa_key.as_deref().map(str::trim), mnemonic) {
            (Some(k), _) if !k.is_empty() => (
                Some(EoaSigner::from_str(k).map_err(|e| anyhow!("invalid EOA key: {e}"))?),
                Some("imported key".to_string()),
            ),
            (_, Some(m)) if !m.trim().is_empty() => {
                let key = keys::derive_ethereum_key(m, p.index)?;
                (
                    Some(
                        EoaSigner::from_bytes(&key.into())
                            .map_err(|e| anyhow!("derived Ethereum key: {e}"))?,
                    ),
                    Some(keys::ethereum_path(p.index)),
                )
            }
            _ => (None, None),
        };

        let rpc_url = match p.rpc_url.as_deref().map(str::trim) {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => default_rpc_url(chain.id)
                .ok_or_else(|| anyhow!("no default RPC for chain {}", chain.id))?
                .to_string(),
        };
        if !(rpc_url.starts_with("http://") || rpc_url.starts_with("https://")
            || rpc_url.starts_with("ws://") || rpc_url.starts_with("wss://"))
        {
            bail!("RPC URL must start with http(s):// or ws(s)://, got \"{rpc_url}\"");
        }

        let provider = match &eoa {
            Some(eoa) => ProviderBuilder::new()
                .network::<Ethereum>()
                .wallet(eoa.clone())
                .connect(&rpc_url)
                .await
                .context("connecting to RPC")?
                .erased(),
            None => ProviderBuilder::new()
                .network::<Ethereum>()
                .connect(&rpc_url)
                .await
                .context("connecting to RPC")?
                .erased(),
        };

        let rpc_chain = provider.get_chain_id().await.context("eth_chainId")?;
        if rpc_chain != chain.id {
            bail!("RPC is on chain {rpc_chain}, expected {}", chain.id);
        }

        // Keyed by the master public key: chain-independent, and stable even if the address
        // encoding changes again. The chain id already namespaces the directory one level up.
        let master_key = signer.address().master_key().to_string();
        let tag = hex::encode(&Sha256::digest(master_key.as_bytes())[..8]);
        let chain_dir = self.base_dir.join(chain.id.to_string());
        let data_dir = chain_dir.join(&tag);

        // Directories from before this scheme were keyed by the chain-specific address; rename
        // so existing wallets keep their synced state instead of resyncing from scratch.
        // Ledger wallets postdate the scheme: nothing to migrate.
        if let Some(keys) = &keys {
            let legacy_address = RailgunAddress::from_private_keys(
                keys.spending,
                keys.viewing,
                RgChainId::evm(chain.id),
            )
            .to_string();
            let legacy_dir =
                chain_dir.join(hex::encode(&Sha256::digest(legacy_address.as_bytes())[..8]));
            if legacy_dir.is_dir() && !data_dir.exists() {
                std::fs::rename(&legacy_dir, &data_dir).with_context(|| {
                    format!("migrating {} to {}", legacy_dir.display(), data_dir.display())
                })?;
                info!(from = %legacy_dir.display(), to = %data_dir.display(), "wallet directory migrated");
            }
        }
        let state = db::open(&data_dir)?;

        let mut builder =
            RailgunBuilder::new(chain.clone(), provider.clone()).with_state(state.clone());
        if p.poi {
            builder = builder.with_poi();
        }
        let mut railgun = builder.build().await.context("building railgun provider")?;
        railgun.register(signer.clone()).await?;
        // Registering records the account in the state. Saving now also proves the directory is
        // writable before anything is synced or proved.
        db::save(&data_dir, &state).context("saving the wallet state")?;

        let bundler_url = p
            .bundler_url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| default_bundler_url(chain.id));

        let waku_url = p
            .waku_url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_WAKU_URL.to_string());
        // "none" is how the front says the field was cleared on purpose (test networks).
        let trusted_signers: Vec<String> = p
            .trusted_fee_signers
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.eq_ignore_ascii_case("none"))
            .unwrap_or("")
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect();
        let waku_mode = match p.waku_mode.as_deref().map(str::trim) {
            None | Some("") | Some("native") if NATIVE_WAKU => "native",
            None | Some("") | Some("native") => {
                warn!("this build has no native Waku node, using the node of the page");
                "browser"
            }
            Some("browser") => "browser",
            Some("nwaku") => "nwaku",
            Some(other) => {
                bail!("unknown Waku mode \"{other}\" (expected native, browser or nwaku)")
            }
        };
        let transport: Arc<dyn WakuTransport> = match waku_mode {
            "browser" => self.shared.bridge.clone(),
            "nwaku" => Arc::new(NwakuRest::new(waku_url.clone())),
            _ => native_transport(chain.id),
        };
        let broadcaster = Arc::new(if trusted_signers.is_empty() {
            BroadcasterClient::new(transport, chain.id)
        } else {
            BroadcasterClient::with_trusted_signers(transport, chain.id, &trusted_signers)
                .map_err(|e| anyhow!("{e}"))?
        });
        // Replaces the monitor of a previous session, if any.
        self.session = None;
        let fee_monitor = self
            .io
            .spawn(fee_monitor(
                broadcaster.clone(),
                self.shared.clone(),
                waku_mode,
                waku_url,
            ))
            .abort_handle();

        info!(%address, chain = chain.id, poi = p.poi, rpc = %rpc_url, "wallet unlocked");
        self.session = Some(Session {
            chain,
            provider,
            eoa,
            eoa_source,
            signer,
            railgun,
            state,
            derivation,
            bundler_url,
            data_dir,
            tokens: HashMap::new(),
            broadcaster,
            bridge: (waku_mode == "browser").then(|| self.shared.bridge.clone()),
            silent_broadcasters: Vec::new(),
            fee_limits: FeeLimits::default(),
            io: self.io.clone(),
            fee_monitor,
        });
        self.refresh_snapshot().await;
        Ok(())
    }

    async fn run(&mut self, ctx: &JobCtx<'_>, op: Op) -> Result<Value> {
        let s = self
            .session
            .as_mut()
            .ok_or_else(|| anyhow!("wallet is locked"))?;
        match op {
            Op::Sync => {
                ctx.step("syncing UTXO tree, TXID tree and pending POI proofs");
                s.railgun.sync().await?;
                Ok(json!({ "syncedBlock": s.railgun.synced_block() }))
            }
            Op::Shield { asset, amount } => s.shield(ctx, &asset, &amount).await,
            Op::Transfer {
                to,
                asset,
                amount,
                memo,
                transport,
                fee_limits,
            } => {
                s.fee_limits = fee_limits;
                let (to, encryption) = parse_recipient(&to)?;
                if encryption != NoteEncryption::Legacy {
                    ctx.step("note to the recipient encrypted with ML-KEM-768 + ECDH (post-quantum)");
                }
                let (asset, value, _) = s.resolve_amount(&asset, &amount).await?;
                let intents = AmountIntents::new().transfer_with_encryption(
                    s.signer.clone() as Arc<dyn RailgunSigner>,
                    to,
                    asset,
                    value,
                    &memo,
                    encryption,
                );
                s.submit(ctx, intents, transport, Vec::new(), None, None).await
            }
            Op::Unshield {
                to,
                asset,
                amount,
                native,
                transport,
                fee_limits,
            } => {
                s.fee_limits = fee_limits;
                let to = Address::from_str(to.trim())
                    .map_err(|e| anyhow!("invalid 0x recipient: {e}"))?;
                s.unshield(ctx, to, &asset, &amount, native, transport).await
            }
        }
    }

    async fn refresh_snapshot(&mut self) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        let snapshot = s.snapshot().await;
        if let Ok(mut status) = self.shared.status.write() {
            *status = snapshot;
        }
    }
}

impl Session {
    /// Resolves the front's asset spec (`"native"` or an ERC-20 address) and a decimal amount
    /// into an `AssetId` and base units. Shielded native currency is the wrapped base token.
    async fn resolve_amount(&mut self, asset: &str, amount: &str) -> Result<(AssetId, u128, Address)> {
        let token = if asset.trim().eq_ignore_ascii_case("native") {
            self.chain.wrapped_base_token
        } else {
            Address::from_str(asset.trim()).map_err(|e| anyhow!("invalid token address: {e}"))?
        };
        let meta = self.token_meta(token).await;
        let value: U256 = parse_units(amount.trim(), meta.decimals)
            .map_err(|e| anyhow!("invalid amount: {e}"))?
            .get_absolute();
        let value = u128::try_from(value).map_err(|_| anyhow!("amount too large"))?;
        if value == 0 {
            bail!("amount must be positive");
        }
        Ok((AssetId::Erc20(token), value, token))
    }

    async fn token_meta(&mut self, token: Address) -> TokenMeta {
        if let Some(meta) = self.tokens.get(&token) {
            return meta.clone();
        }
        let erc20 = ERC20::new(token, self.provider.clone());
        let decimals = erc20.decimals().call().await;
        let symbol = erc20.symbol().call().await;
        let complete = decimals.is_ok() && symbol.is_ok();
        let meta = TokenMeta {
            symbol: symbol.unwrap_or_else(|_| "?".into()),
            decimals: decimals.unwrap_or(18),
        };
        // Only cache real answers, a transient RPC error must not pin 18 decimals forever.
        if complete {
            self.tokens.insert(token, meta.clone());
        } else {
            warn!(%token, "could not read ERC-20 metadata, assuming 18 decimals");
        }
        meta
    }

    async fn send_eoa(&self, ctx: &JobCtx<'_>, label: &str, tx: TransactionRequest) -> Result<B256> {
        if self.eoa.is_none() {
            bail!("no public EOA key configured, cannot send {label} transaction");
        }
        ctx.step(format!("sending {label} transaction from EOA"));
        let pending = self
            .provider
            .send_transaction(tx)
            .await
            .with_context(|| format!("{label}: eth_sendTransaction"))?;
        let hash = *pending.tx_hash();
        ctx.step(format!("{label} tx {hash} sent, waiting for inclusion"));
        let receipt = pending
            .get_receipt()
            .await
            .with_context(|| format!("{label}: waiting for receipt"))?;
        if !receipt.status() {
            bail!("{label} transaction {hash} reverted");
        }
        ctx.step(format!(
            "{label} included in block {}",
            receipt.block_number.unwrap_or_default()
        ));
        Ok(hash)
    }

    async fn shield(&mut self, ctx: &JobCtx<'_>, asset: &str, amount: &str) -> Result<Value> {
        let eoa = self
            .eoa
            .as_ref()
            .map(EoaSigner::address)
            .ok_or_else(|| anyhow!("shielding needs a public EOA key"))?;
        let is_native = asset.trim().eq_ignore_ascii_case("native");
        let (asset_id, value, token) = self.resolve_amount(asset, amount).await?;
        let recipient = self.signer.address();

        let mut hashes = Vec::new();
        let txs = if is_native {
            self.railgun
                .shield()
                .shield_native(recipient, value)
                .build(&mut rand::rng())?
        } else {
            let erc20 = ERC20::new(token, self.provider.clone());
            let spender = self.chain.railgun_smart_wallet;
            let balance = erc20.balanceOf(eoa).call().await?;
            if balance < U256::from(value) {
                bail!("EOA token balance {balance} is below the shield amount {value}");
            }
            let allowance = erc20.allowance(eoa, spender).call().await?;
            if allowance < U256::from(value) {
                let approve = TransactionRequest::default()
                    .to(token)
                    .input(
                        ERC20::approveCall {
                            spender,
                            amount: U256::from(value),
                        }
                        .abi_encode()
                        .into(),
                    )
                    .value(U256::ZERO);
                hashes.push(self.send_eoa(ctx, "approve", approve).await?);
            }
            self.railgun
                .shield()
                .shield(recipient, asset_id, value)
                .build(&mut rand::rng())?
        };

        for tx in txs {
            hashes.push(self.send_eoa(ctx, "shield", tx).await?);
        }
        self.post_tx_sync(ctx).await;
        Ok(json!({ "txHashes": hashes.iter().map(|h| h.to_string()).collect::<Vec<_>>() }))
    }

    async fn unshield(
        &mut self,
        ctx: &JobCtx<'_>,
        to: Address,
        asset: &str,
        amount: &str,
        native: bool,
        transport: Transport,
    ) -> Result<Value> {
        let (asset_id, value, token) = self.resolve_amount(asset, amount).await?;
        let signer = self.signer.clone() as Arc<dyn RailgunSigner>;

        if !native {
            let intents = AmountIntents::new().unshield(signer, to, asset_id, value)?;
            return self.submit(ctx, intents, transport, Vec::new(), None, None).await;
        }

        if token != self.chain.wrapped_base_token {
            bail!("native unshield only applies to the wrapped base token");
        }
        if transport != Transport::Erc4337 {
            // Legacy and direct: the transaction unshields the wrapped token to the RelayAdapt
            // contract, which unwraps its whole balance and forwards the native currency in the
            // same EVM transaction (RelayAdapt.relay). The recipient and the unwrap are bound
            // into the proofs through adaptParams: whoever submits carries bytes it cannot alter.
            // `submit` binds the action once the spends are final (fee note included).
            let relay_adapt = self.chain.relay_adapt_contract;
            let mut action = RelayAction::unshield_base_token(relay_adapt, to, &mut rand::rng());
            // As the community engine: all or nothing when we send it ourselves; through a
            // broadcaster the Railgun transaction must land even if a call fails, since the
            // fee is paid either way.
            action.require_success = transport == Transport::Direct;
            let intents = AmountIntents::new().unshield(signer, relay_adapt, asset_id, value)?;
            return self.submit(ctx, intents, transport, Vec::new(), None, Some(action)).await;
        }

        // The wrapped token lands on the ephemeral 7702 sender, which unwraps it and forwards the
        // native currency during the execution phase of the same UserOperation.
        let sender = EoaSigner::random();
        let fee = value * u128::from(self.chain.unshield_fee_bps) / 10_000;
        let received = value - fee;
        let calls = vec![
            Call {
                target: token,
                value: U256::ZERO,
                data: WETH9::withdrawCall {
                    wad: U256::from(received),
                }
                .abi_encode()
                .into(),
            },
            Call {
                target: to,
                value: U256::from(received),
                data: Default::default(),
            },
        ];
        let intents = AmountIntents::new().unshield(signer, sender.address(), asset_id, value)?;
        self.submit(ctx, intents, transport, calls, Some(sender), None).await
    }

    /// Selects the notes for `intents`, then proves and submits the transaction over the
    /// selected transport. `relay` is a RelayAdapt action to bind into the spends, legacy and
    /// direct transports only: the proved transaction is then sent as `RelayAdapt.relay(...)`.
    /// It is bound after the spends are final (after a broadcaster fee is attached), since the
    /// adapt params commit to the nullifiers of every input.
    async fn submit(
        &mut self,
        ctx: &JobCtx<'_>,
        intents: AmountIntents,
        transport: Transport,
        calls: Vec<Call>,
        sender: Option<EoaSigner>,
        relay: Option<RelayAction>,
    ) -> Result<Value> {
        if relay.is_some() && transport == Transport::Erc4337 {
            bail!("RelayAdapt actions are for the legacy and direct transports");
        }
        let intents = if self.fee_limits.hybrid_change {
            ctx.step("change notes encrypted with ML-KEM-768 + ECDH, to this wallet's key");
            intents.with_hybrid_change()
        } else {
            intents
        };
        let spends = self.railgun.resolve_amounts(intents, &mut rand::rng()).await?;
        let result = match transport {
            Transport::Legacy => {
                if !calls.is_empty() {
                    bail!(
                        "the legacy transport runs post-transaction calls through RelayAdapt \
                         (RelayAction), not through a sender account"
                    );
                }
                self.submit_legacy(ctx, &spends, relay.as_ref()).await?
            }
            Transport::Direct => {
                if self.eoa.is_none() {
                    bail!("direct transport needs a public EOA key");
                }
                // No fee note here, so the spends are final: bind the action now.
                let spends = match &relay {
                    Some(action) => action.bind(spends),
                    None => spends,
                };
                // Exercises the dummy-proof path the legacy transport will use for fee quotes:
                // same calldata shape, zero proof, estimated from the verification bypass origin.
                match self.estimate_with_dummy_proof(&spends, None, relay.as_ref()).await {
                    Ok(gas) => ctx.step(format!("dummy-proof gas estimate: {gas}")),
                    Err(e) => ctx.step(format!("dummy-proof gas estimate failed (non fatal): {e:#}")),
                }
                ctx.step("proving (direct transport)");
                let mut proved = self.railgun.prove(&spends, &mut rand::rng()).await?;
                if let Some(action) = &relay {
                    action.package(&mut proved);
                }
                ctx.step(format!("proved {} operation(s)", proved.proved_operations.len()));
                let hash = self.send_eoa(ctx, "transact", proved.tx_data).await?;
                json!({ "transport": "direct", "txHash": hash.to_string() })
            }
            Transport::Erc4337 => {
                let sender = sender.unwrap_or_else(EoaSigner::random);
                let funds_transit = !calls.is_empty();
                if funds_transit {
                    // If the execution phase reverts after the paymaster phase unshielded, the
                    // funds sit on this address. Keep the key before anything is sent.
                    record_ephemeral_sender(&self.data_dir, &sender)?;
                }
                let bundler = PimlicoBundler::new(
                    self.bundler_url
                        .parse()
                        .map_err(|e| anyhow!("invalid bundler url: {e}"))?,
                );
                let account =
                    SimpleSmartAccount::new(sender.address(), self.chain.id, Arc::new(self.provider.clone()));
                let railgun_signer = self.signer.clone() as Arc<dyn RailgunSigner>;
                let fee_token = self.chain.wrapped_base_token;
                let (mut signable, mode) = if self.fee_limits.single_proof {
                    // The bundler estimates the dummy-proof operation itself, with the Railgun
                    // verifier told to accept the dummy proof through a state override: the
                    // limits are its own figures for the exact operation, nothing is signed.
                    ctx.step("asking the bundler to estimate the dummy-proof operation");
                    let estimate = self
                        .railgun
                        .simulate_gas_limits(
                            &spends,
                            &bundler as &dyn Bundler,
                            &account,
                            railgun_signer.clone(),
                            fee_token,
                            &calls,
                            &mut rand::rng(),
                        )
                        .await
                        .map_err(anyhow::Error::from);
                    match estimate {
                        Ok(gas) => {
                            ctx.step(format!(
                                "limits from the bundler's estimate of the dummy-proof operation: pre-verification {}, account verification {}, call {}, paymaster verification {}, post-op {}, at {} wei/gas; proving once from ephemeral sender {}",
                                gas.pre_verification_gas,
                                gas.verification_gas_limit,
                                gas.call_gas_limit,
                                gas.paymaster_verification_gas_limit.unwrap_or(0),
                                gas.paymaster_post_op_gas_limit.unwrap_or(0),
                                gas.max_fee_per_gas,
                                sender.address()
                            ));
                            let (signable, report) = self
                                .railgun
                                .prepare_userop_single_proof(
                                    &spends,
                                    &bundler as &dyn Bundler,
                                    &account,
                                    railgun_signer,
                                    fee_token,
                                    calls,
                                    gas,
                                    &mut rand::rng(),
                                )
                                .await?;
                            ctx.step(format!(
                                "proved once: paymaster verification measured {} of {} allowed",
                                report.paymaster_verification_measured,
                                report.paymaster_verification_limit
                            ));
                            (signable, "single-proof")
                        }
                        Err(e) if self.fee_limits.single_proof_strict => {
                            return Err(e.context(
                                "the bundler could not estimate the dummy-proof operation and \
                                 falling back to several signatures is disabled. Nothing was \
                                 signed",
                            ));
                        }
                        Err(e) => {
                            ctx.step(format!(
                                "the bundler could not estimate the dummy-proof operation ({e:#}): falling back to the iterative estimate, which signs several times"
                            ));
                            let signable = self
                                .railgun
                                .prepare_userop(
                                    &spends,
                                    &bundler as &dyn Bundler,
                                    &account,
                                    railgun_signer,
                                    fee_token,
                                    calls,
                                    &mut rand::rng(),
                                )
                                .await?;
                            (signable, "iterative")
                        }
                    }
                } else {
                    ctx.step(format!(
                        "preparing UserOperation from ephemeral sender {} (proof is regenerated until the fee converges)",
                        sender.address()
                    ));
                    let signable = self
                        .railgun
                        .prepare_userop(
                            &spends,
                            &bundler as &dyn Bundler,
                            &account,
                            railgun_signer,
                            fee_token,
                            calls,
                            &mut rand::rng(),
                        )
                        .await?;
                    (signable, "iterative")
                };
                let max_fee = signable.total_gas_limit() * signable.user_op.max_fee_per_gas;
                ctx.step(format!(
                    "fee {} wrapped token ({mode})",
                    trim_decimal(format_units(U256::from(max_fee), 18u8).unwrap_or_default())
                ));
                if let Some(warning) = refresh_priority_fee(&bundler, &mut signable).await {
                    ctx.step(warning);
                }
                let signed = signable.sign(&sender).await?;
                let hash = bundler.send_user_operation(&signed).await?;
                ctx.step(format!("UserOperation {:?} sent, waiting for receipt", hash.0));
                let receipt = bundler.wait_for_receipt(hash).await?;
                if !receipt.success {
                    bail!("UserOperation {:?} failed on-chain: {receipt:?}", hash.0);
                }
                json!({
                    "transport": "erc4337",
                    "userOpHash": format!("{:?}", hash.0),
                    "sender": sender.address().to_string(),
                    "maxFeeWei": max_fee.to_string(),
                    "feeMode": mode,
                })
            }
        };
        self.post_tx_sync(ctx).await;
        Ok(result)
    }

    /// Relays through a Railgun community broadcaster: quote, fee from a dummy-proof gas
    /// estimate, proof with the fee note pinned first and `minGasPrice` bound, pre-transaction
    /// POIs, sealed request over Waku.
    async fn submit_legacy(
        &mut self,
        ctx: &JobCtx<'_>,
        spends: &[Spend],
        relay: Option<&RelayAction>,
    ) -> Result<Value> {
        // Real funds: never pay an uncapped rate. The reference client makes the trusted signer
        // mandatory everywhere; test networks stay usable without one.
        if self.chain.id == 1 && self.broadcaster.trusted_signer_count() == 0 {
            bail!(
                "on mainnet the legacy transport needs a trusted fee signer: reopen the wallet \
                 with the 0zk address of one, so broadcaster rates are capped"
            );
        }
        let fee_token = self.chain.wrapped_base_token;
        let fee_asset = AssetId::Erc20(fee_token);
        let list_keys = self.railgun.poi_list_keys();

        let rate_multiple = self
            .fee_limits
            .max_fee_rate
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_MAX_FEE_RATE)
            .to_string();
        // "1.5" with 18 decimals is 1.5 times the par rate of the wrapped base token.
        let max_rate: u128 = parse_units(&rate_multiple, 18u8)
            .ok()
            .and_then(|v| u128::try_from(v.get_absolute()).ok())
            .filter(|v| *v >= PAR_RATE_WRAPPED_BASE_TOKEN / 2)
            .ok_or_else(|| anyhow!("invalid maximum fee rate \"{rate_multiple}\" (expected a multiple of the gas cost such as 1.5)"))?;
        let max_fee: Option<u128> = match self.fee_limits.max_fee.as_deref().map(str::trim) {
            Some(v) if !v.is_empty() => Some(
                parse_units(v, 18u8)
                    .ok()
                    .and_then(|v| u128::try_from(v.get_absolute()).ok())
                    .ok_or_else(|| anyhow!("invalid maximum fee \"{v}\""))?,
            ),
            _ => None,
        };

        self.silent_broadcasters
            .retain(|(_, since)| since.elapsed() < SILENT_BROADCASTER_PENALTY);
        let chosen = self
            .fee_limits
            .broadcaster
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_string);
        let selection = match &chosen {
            // An explicit choice is honoured even if that broadcaster was silent before: that
            // is how one tests a given broadcaster.
            Some(address) => self
                .broadcaster
                .quotes_for(&fee_token.to_string(), &list_keys)
                .into_iter()
                .find(|q| &q.railgun_address == address && q.fee_per_unit_gas <= max_rate)
                .ok_or(NoQuote::None),
            None => {
                let exclude: Vec<String> =
                    self.silent_broadcasters.iter().map(|(a, _)| a.clone()).collect();
                self.broadcaster.select_quote(
                    &fee_token.to_string(),
                    &list_keys,
                    Some(max_rate),
                    BROADCASTER_DRAW_PERCENT,
                    &exclude,
                    |n| {
                        use rand::RngExt;
                        rand::rng().random_range(0..n.max(1))
                    },
                )
            }
        };
        let quote: FeeQuote = match selection {
            Ok(quote) => quote,
            Err(NoQuote::AboveCeiling { cheapest, rejected, .. }) => bail!(
                "{rejected} broadcaster offer(s), the cheapest at {} times the gas cost, above \
                 your ceiling of {rate_multiple}. Nothing was sent. Raise the ceiling only if you \
                 accept that price.",
                format_rate(cheapest)
            ),
            Err(e @ NoQuote::PoiListMismatch { .. }) => bail!("{e}. Nothing was sent."),
            Err(NoQuote::None) if chosen.is_some() => bail!(
                "the chosen broadcaster has no usable offer right now (expired, over your ceiling, \
                 or outside the trusted band). Clear the choice to draw among the others."
            ),
            Err(NoQuote::None) => bail!(
                "no usable broadcaster offer for the wrapped base token. Either the Waku node \
                 is not on the Railgun shard, or a trusted fee signer is set and has not \
                 announced a rate yet, or every offer is outside its band"
            ),
        };
        let broadcaster_address = RailgunAddress::from_str(&quote.railgun_address)
            .map_err(|e| anyhow!("broadcaster address: {e}"))?;
        ctx.step(format!(
            "broadcaster {}… ({}), rate {} times the gas cost (ceiling {rate_multiple}), quote valid {}s",
            &quote.railgun_address[..quote.railgun_address.len().min(14)],
            quote.identifier.as_deref().unwrap_or("no identifier"),
            format_rate(quote.fee_per_unit_gas),
            quote.expiration.saturating_sub(quote_now_ms()) / 1000,
        ));

        // Broadcasters send type 1 transactions at exactly minGasPrice and refuse a price
        // under half the slow market price: take the node's price plus 10%.
        let gas_price = self.provider.get_gas_price().await.context("eth_gasPrice")? * 11 / 10;

        let fee_for = |gas: u64| {
            token_fee(quote.fee_per_unit_gas, gas, gas_price)
                .ok_or_else(|| anyhow!("fee overflow"))
        };

        // The fee changes note values, possibly the number of inputs, hence the gas: estimate
        // twice, the second time with a fee of the right magnitude.
        let mut fee = fee_for(FIRST_GUESS_GAS)?;
        let mut gas = 0;
        for round in 1..=2 {
            let with_fee = self
                .spends_for_broadcaster(spends, broadcaster_address, fee_asset, fee, gas_price, relay)
                .await?;
            gas = self
                .estimate_with_dummy_proof(&with_fee, Some(gas_price), relay)
                .await
                .with_context(|| format!("dummy-proof gas estimate (round {round})"))?;
            fee = fee_for(gas)?;
        }
        ctx.step(format!(
            "gas estimate {gas}, gas price {gas_price} wei, broadcaster fee {} wrapped token",
            trim_decimal(format_units(U256::from(fee), 18u8).unwrap_or_default())
        ));

        if let Some(max_fee) = max_fee {
            if fee > max_fee {
                bail!(
                    "the broadcaster fee would be {} wrapped token, above your maximum of {}. \
                     Nothing was proved or sent.",
                    trim_decimal(format_units(U256::from(fee), 18u8).unwrap_or_default()),
                    trim_decimal(format_units(U256::from(max_fee), 18u8).unwrap_or_default()),
                );
            }
        }

        ctx.step("proving (legacy transport)");
        let with_fee = self
            .spends_for_broadcaster(spends, broadcaster_address, fee_asset, fee, gas_price, relay)
            .await?;
        let mut proved = self.railgun.prove(&with_fee, &mut rand::rng()).await?;
        if let Some(action) = relay {
            action.package(&mut proved);
        }
        ctx.step(format!(
            "proved {} operation(s), generating pre-transaction POI",
            proved.proved_operations.len()
        ));
        let pre_transaction_pois =
            railgun_broadcaster::pre_transaction_pois(&self.railgun, &proved.proved_operations)
                .await?;

        if !quote.usable_at(quote_now_ms().saturating_sub(30_000)) {
            bail!("the fee quote expired while proving, run the operation again");
        }

        let use_relay_adapt = relay.is_some();
        if use_relay_adapt {
            ctx.step("submitting through RelayAdapt.relay (unwrap and recipient bound in adaptParams)");
        }
        let (to, calldata, _) = railgun_broadcaster::relay_adapt::call_parts(&proved.tx_data)
            .ok_or_else(|| anyhow!("the proved transaction has no target"))?;
        let sealed = self.broadcaster.seal(
            BroadcastRequest {
                quote: quote.clone(),
                to: to.to_checksum(None),
                calldata: calldata.to_vec(),
                min_gas_price: gas_price,
                use_relay_adapt,
                pre_transaction_pois,
            },
            &mut rand::rng(),
        )?;

        let stats_before = self.broadcaster.publish_stats();
        ctx.step("request sealed and published, waiting for the broadcaster (up to 120s)");
        let client = self.broadcaster.clone();
        let outcome = self
            .io
            .spawn(async move { client.send(&sealed).await })
            .await
            .context("waku task")?;
        match outcome {
            Ok(tx_hash) => {
                ctx.step(format!("broadcaster sent transaction {tx_hash}"));
                Ok(json!({
                    "transport": "legacy",
                    "txHash": tx_hash,
                    "broadcaster": quote.railgun_address,
                    "fee": fee.to_string(),
                    "minGasPrice": gas_price.to_string(),
                }))
            }
            Err(ClientError::Timeout(_)) => {
                // Did the request leave the tab at all? A light push that fails looks the same
                // as a broadcaster that stays silent, and the remedy is not.
                let delivery = match (self.broadcaster.publish_stats(), stats_before) {
                    (Some(after), Some(before)) => {
                        let delivered = after.delivered - before.delivered;
                        let failed = after.failed - before.failed;
                        let unacked = (after.queued - before.queued).saturating_sub(delivered + failed);
                        let via = if self.bridge.is_some() { "through the tab" } else { "by the native node" };
                        ctx.step(format!(
                            "publishes {via}: {delivered} accepted by a Waku peer, {failed} failed{}{}",
                            after.last_error.as_ref().filter(|_| failed > 0).map(|e| format!(" ({e})")).unwrap_or_default(),
                            if unacked > 0 { format!(", {unacked} never acknowledged") } else { String::new() }
                        ));
                        Some(delivered)
                    }
                    _ => None,
                };
                if delivery != Some(0) {
                    // It was delivered (or we cannot tell): the broadcaster is the silent party.
                    self.silent_broadcasters
                        .push((quote.railgun_address.clone(), std::time::Instant::now()));
                }
                // No answer is not an outcome. The chain is: a spent input means the
                // transaction was mined, whoever sent it.
                ctx.step("no answer from the broadcaster within 120s: checking on-chain whether the inputs were spent");
                let inputs: Vec<(u32, u32)> = proved
                    .proved_operations
                    .iter()
                    .flat_map(|op| op.inner.in_notes().iter().map(|n| (n.tree_number, n.leaf_index)))
                    .collect();
                let address = self.signer.address();
                for attempt in 1..=4 {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    if let Err(e) = self.railgun.sync().await {
                        ctx.step(format!("sync {attempt}/4 failed: {e}"));
                        continue;
                    }
                    let unspent = self.railgun.notes(address).await;
                    let still_there = inputs
                        .iter()
                        .filter(|(t, l)| unspent.iter().any(|n| n.tree_number == *t && n.leaf_index == *l))
                        .count();
                    if still_there == 0 {
                        ctx.step("the input notes are spent: the transaction was mined, the broadcaster's answer was lost");
                        return Ok(json!({
                            "transport": "legacy",
                            "txHash": null,
                            "outcome": "mined, answer lost",
                            "broadcaster": quote.railgun_address,
                            "fee": fee.to_string(),
                        }));
                    }
                    ctx.step(format!("check {attempt}/4: {still_there} of {} input note(s) still unspent", inputs.len()));
                }
                let dropped = self
                    .railgun
                    .discard_pending_poi(&proved.proved_operations)
                    .await
                    .unwrap_or(0);
                bail!(
                    "the broadcaster never answered and, 80s later, the input notes are still \
                     unspent: the transaction was not sent. Nothing was paid. {dropped} pending \
                     POI entr{} dropped. {}",
                    if dropped == 1 { "y" } else { "ies" },
                    if delivery == Some(0) && self.bridge.is_some() {
                        "The request never left this tab: reload the page so its Waku node \
                         reconnects, then retry."
                    } else if delivery == Some(0) {
                        "No Waku peer accepted the request: check the state of the native Waku \
                         node in the legacy panel and the log, then retry."
                    } else {
                        "The request was delivered to the Waku network, so this broadcaster is \
                         the silent party: it is left out of the draw for 10 minutes, retry to \
                         use another one."
                    }
                )
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The spends as the broadcaster will relay them: the fee note attached (first commitment
    /// of the first operation, the only one broadcasters look at), `minGasPrice` on every spend
    /// since the contract checks it per transaction, and the relay action bound last, so that
    /// the adapt params commit to the nullifiers of the fee spend too.
    async fn spends_for_broadcaster(
        &mut self,
        spends: &[Spend],
        broadcaster: RailgunAddress,
        fee_asset: AssetId,
        fee: u128,
        gas_price: u128,
        relay: Option<&RelayAction>,
    ) -> Result<Vec<Spend>> {
        let signer = self.signer.clone() as Arc<dyn RailgunSigner>;
        let with_fee: Vec<Spend> = self
            .railgun
            .attach_broadcaster_fee(spends, signer, broadcaster, fee_asset, fee, &mut rand::rng())
            .await?
            .into_iter()
            .map(|spend| spend.min_gas_price(gas_price))
            .collect();
        Ok(match relay {
            Some(action) => action.bind(with_fee),
            None => with_fee,
        })
    }

    /// `gas_price` must be the `minGasPrice` bound in the spends, if any: the Railgun contract
    /// compares it to `tx.gasprice`, which is 0 in an estimate that names no price, and reverts
    /// with "Gas price too low". The estimate then runs as the legacy (type 0) transaction the
    /// broadcaster will send. `relay` must already be bound into `spends`; it is only used to
    /// package the dummy transaction as the `RelayAdapt.relay` call.
    async fn estimate_with_dummy_proof(
        &mut self,
        spends: &[Spend],
        gas_price: Option<u128>,
        relay: Option<&RelayAction>,
    ) -> Result<u64> {
        let mut dummy = self.railgun.prove_dummy(spends, &mut rand::rng()).await?;
        if let Some(action) = relay {
            action.package(&mut dummy);
        }
        let mut request = dummy.tx_data;
        request = request.from(railgun::provider::VERIFICATION_BYPASS);
        if let Some(gas_price) = gas_price {
            request = request.gas_price(gas_price);
        }
        match self.provider.estimate_gas(request.clone()).await {
            Ok(gas) => Ok(gas),
            // With a gas price, the node caps the gas by the sender's balance, and the bypass
            // address may hold nothing on this chain. Retry with a balance override.
            Err(e) if e.to_string().to_lowercase().contains("insufficient funds") => {
                use alloy::rpc::types::state::{AccountOverride, StateOverride};
                let mut overrides = StateOverride::default();
                overrides.insert(
                    railgun::provider::VERIFICATION_BYPASS,
                    AccountOverride::default().with_balance(U256::from(10u128.pow(24))),
                );
                Ok(self
                    .provider
                    .estimate_gas(request)
                    .overrides(overrides)
                    .await
                    .context("gas estimate with balance override")?)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn post_tx_sync(&mut self, ctx: &JobCtx<'_>) {
        ctx.step(format!(
            "waiting {}s for indexing, then syncing",
            POST_TX_SYNC_DELAY.as_secs()
        ));
        tokio::time::sleep(POST_TX_SYNC_DELAY).await;
        if let Err(e) = self.railgun.sync().await {
            warn!("post-transaction sync failed (run a manual sync): {e}");
            ctx.step(format!("post-transaction sync failed: {e}"));
        }
        // The proof's pending POI entries and the new notes are on disk before the job ends.
        if let Err(e) = self.persist() {
            warn!("saving the wallet state failed: {e:#}");
            ctx.step(format!("saving the wallet state failed: {e:#}"));
        }
    }

    /// Writes a snapshot of the SDK state to `data_dir`, see `db`.
    fn persist(&self) -> Result<()> {
        db::save(&self.data_dir, &self.state)
    }

    async fn snapshot(&mut self) -> StatusSnapshot {
        let address = self.signer.address();

        let mut balances = Vec::new();
        for entry in self.railgun.balance(address).await {
            let mut view = json!({
                "asset": entry.asset.to_string(),
                "amount": entry.amount.to_string(),
                "poiStatus": entry.poi_status,
            });
            if let AssetId::Erc20(token) = entry.asset {
                let meta = self.token_meta(token).await;
                view["token"] = json!(token.to_string());
                view["symbol"] = json!(meta.symbol);
                view["decimals"] = json!(meta.decimals);
                view["formatted"] = json!(trim_decimal(
                    format_units(U256::from(entry.amount), meta.decimals).unwrap_or_default()
                ));
            }
            balances.push(view);
        }
        balances.sort_by_key(|v| v["asset"].as_str().map(str::to_owned));

        let entries = self.railgun.notes(address).await;
        // Token symbols, read from each token contract through the session's RPC (any chain);
        // cached per token by `token_meta`.
        let mut symbols: HashMap<Address, String> = HashMap::new();
        for n in &entries {
            if let AssetId::Erc20(token) = n.asset {
                if !symbols.contains_key(&token) {
                    let meta = self.token_meta(token).await;
                    symbols.insert(token, meta.symbol);
                }
            }
        }
        let notes = entries
            .into_iter()
            .map(|n| {
                let symbol = match n.asset {
                    AssetId::Erc20(token) => symbols.get(&token).cloned(),
                    _ => None,
                };
                json!({
                    "symbol": symbol,
                    "asset": n.asset.to_string(),
                    "amount": n.amount.to_string(),
                    "poiStatus": n.poi_status,
                    "treeNumber": n.tree_number,
                    "leafIndex": n.leaf_index,
                    "blindedCommitment": n.blinded_commitment,
                    "commitmentType": n.commitment_type,
                    "memo": n.memo,
                })
            })
            .collect();

        let eoa = self.eoa.as_ref().map(EoaSigner::address);
        let eoa_balance = match eoa {
            Some(a) => self
                .provider
                .get_balance(a)
                .await
                .ok()
                .and_then(|b| format_units(b, 18u8).ok())
                .map(trim_decimal),
            None => None,
        };

        let has_paymaster =
            self.chain.privacy_paymaster.is_some() && self.chain.railgun_fee_adapter.is_some();
        let transports = vec![
            TransportInfo {
                id: "erc4337",
                label: "4337",
                enabled: has_paymaster,
                note: if has_paymaster {
                    "UserOperation via privacy paymaster, fee paid in shielded wrapped base token"
                        .into()
                } else {
                    "no privacy paymaster deployed on this chain".into()
                },
            },
            TransportInfo {
                id: "legacy",
                label: "legacy",
                // The front turns it on from the live broadcaster status.
                enabled: false,
                note: "Railgun community broadcasters over Waku, fee paid in shielded wrapped base token".into(),
            },
            TransportInfo {
                id: "direct",
                label: "direct (debug)",
                enabled: eoa.is_some(),
                note: "self-broadcast from the public EOA, links the EOA to the transaction".into(),
            },
        ];

        StatusSnapshot {
            unlocked: true,
            chain_id: Some(self.chain.id),
            address: Some(address.to_string()),
            pq_address: self.signer.kem_key_pair().map(|kem| {
                self.signer
                    .address()
                    .with_kem_key(kem.encapsulation_key().clone())
                    .to_string()
            }),
            derivation: Some(self.derivation.to_string()),
            eoa: eoa.map(|a| a.to_string()),
            eoa_source: self.eoa_source.clone(),
            eoa_balance,
            // The daemon's public account is a host key; the Ledger Ethereum app is web-only.
            eoa_ledger: false,
            poi: self.railgun.poi_enabled(),
            poi_list_keys: self.railgun.poi_list_keys(),
            synced_block: Some(self.railgun.synced_block()),
            wrapped_base_token: Some(self.chain.wrapped_base_token.to_string()),
            railgun_smart_wallet: Some(self.chain.railgun_smart_wallet.to_string()),
            bundler_url: Some(self.bundler_url.clone()),
            transports,
            balances,
            notes,
            poi_pending: self
                .railgun
                .poi_pending()
                .into_iter()
                .filter_map(|p| serde_json::to_value(p).ok())
                .collect(),
            updated_at: now_ms(),
        }
    }
}

/// Margin, in percent, added to the bundler's priority fee right before sending.
const PRIORITY_FEE_MARGIN_PERCENT: u128 = 10;

/// Brings the priority fee of a proved UserOperation up to the bundler's current price.
///
/// The price is read before proving; a proof (two on a re-prove, with Ledger signatures) can take
/// long enough for the bundler's minimum to move (Pimlico refused 1,099,999,959 wei against a
/// minimum of 1,099,999,962). Only `maxFeePerGas` enters the fee bound into the proof, so the
/// priority fee can rise freely up to it without proving again. Returns a step to show when the
/// price no longer fits under that cap.
async fn refresh_priority_fee(
    bundler: &PimlicoBundler,
    signable: &mut userop_kit::signable_user_operation::SignableUserOperation,
) -> Option<String> {
    let Ok(Some(price)) = (bundler as &dyn Bundler).gas_price().await else {
        return None;
    };
    let op = &mut signable.user_op;
    let wanted = price.max_priority_fee_per_gas
        + price.max_priority_fee_per_gas * PRIORITY_FEE_MARGIN_PERCENT / 100;
    op.max_priority_fee_per_gas = wanted.max(op.max_priority_fee_per_gas).min(op.max_fee_per_gas);
    (price.max_priority_fee_per_gas > op.max_fee_per_gas).then(|| {
        format!(
            "the bundler's priority fee ({} wei) is now above the proved max fee ({} wei): it \
             may refuse the operation",
            price.max_priority_fee_per_gas, op.max_fee_per_gas
        )
    })
}

/// Rate as a multiple of the gas cost, two decimals: 1.12e18 -> "1.12".
fn format_rate(rate: u128) -> String {
    let hundredths = rate / (PAR_RATE_WRAPPED_BASE_TOKEN / 100);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// "1.500000" -> "1.5", "2.000" -> "2".
fn trim_decimal(s: String) -> String {
    if !s.contains('.') {
        return s;
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Appends the ephemeral sender key to `<data_dir>/ephemeral_senders.jsonl` (mode 0600).
fn record_ephemeral_sender(data_dir: &Path, sender: &EoaSigner) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("ephemeral_senders.jsonl");
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path).context("opening ephemeral sender log")?;
    let line = json!({
        "ts": now_ms(),
        "address": sender.address().to_string(),
        "privateKey": format!("0x{}", hex::encode(sender.to_bytes())),
    });
    writeln!(file, "{line}")?;
    file.sync_all()?;
    Ok(())
}

/// A version 2 address (with the ML-KEM key) selects the post-quantum scheme for the note, a
/// version 1 address the legacy one.
fn parse_recipient(to: &str) -> Result<(RailgunAddress, NoteEncryption)> {
    let to = to.trim();
    match to.parse::<PqRailgunAddress>() {
        Ok(pq) => Ok((pq.address(), pq.encryption())),
        Err(pq_err) => match RailgunAddress::from_str(to) {
            Ok(address) => Ok((address, NoteEncryption::Legacy)),
            Err(e) => bail!("invalid 0zk recipient: {e} (as a version 2 address: {pq_err})"),
        },
    }
}

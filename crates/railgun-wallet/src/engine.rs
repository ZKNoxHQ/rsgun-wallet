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
    providers::{DynProvider, Provider, ProviderBuilder},
    signers::local::PrivateKeySigner as EoaSigner,
    sol,
    sol_types::SolCall,
};
use anyhow::{Context, Result, anyhow, bail};
use eip_1193_provider::tx_data::TxData;
use railgun::{
    account::{
        address::RailgunAddress,
        chain::ChainId as RgChainId,
        signer::{PrivateKeySigner as RgSigner, RailgunSigner},
    },
    builder::RailgunBuilder,
    caip::AssetId,
    chain_config::ChainConfig,
    crypto::keys::SpendingPublicKey,
    provider::RailgunProvider,
    transact::TransactionBuilder,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use userop_kit::{
    bundler::{Bundler, pimlico::PimlicoBundler},
    user_operation::UserOperationGasEstimate,
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

/// Margin on the simulated limits. The dominant one, the paymaster validation, needs 8.0 to
/// 8.4% more than the gas the probe sees it use (63/64 forwarding over the nested calls; four
/// live measurements on Sepolia, transfers and unshields). 12% leaves about 3.5% above that.
/// The user pays for limits, not for gas used, and nothing is refunded: every point here is a
/// point of fee.
const DEFAULT_GAS_MARGIN_PERCENT: u32 = 12;
const MAX_GAS_MARGIN_PERCENT: u32 = 200;

/// Limits are rounded up to this, so the public fee says less about the transaction.
const GAS_BUCKET: u128 = 10_000;

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
    /// 4337: prove once, with gas limits fixed beforehand from a learned profile plus a margin,
    /// instead of re-proving until the fee converges. One spending signature per operation,
    /// which is what a hardware or threshold signer needs.
    #[serde(default)]
    pub single_proof: bool,
    /// Margin added to the limits in single-proof mode, percent. Defaults to 12.
    pub gas_margin_percent: Option<u32>,
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
    /// Deletes the local database of the open wallet and locks it.
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
        // Closes the database handles and stops the fee monitor before touching the files.
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

        let mut removed = Vec::new();
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
                let signer = RgSigner::new(keys.spending, keys.viewing, RgChainId::All);
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
        let db = db::open(&data_dir)?;

        let mut builder = RailgunBuilder::new(chain.clone(), provider.clone()).with_database(db);
        if p.poi {
            builder = builder.with_poi();
        }
        let mut railgun = builder.build().await.context("building railgun provider")?;
        railgun.register(signer.clone()).await?;

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
                let to = RailgunAddress::from_str(to.trim())
                    .map_err(|e| anyhow!("invalid 0zk recipient: {e}"))?;
                let (asset, value, _) = s.resolve_amount(&asset, &amount).await?;
                let builder = TransactionBuilder::new().transfer(
                    s.signer.clone() as Arc<dyn RailgunSigner>,
                    to,
                    asset,
                    value,
                    &memo,
                );
                s.submit(ctx, builder, transport, Vec::new(), None, None).await
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

    async fn send_eoa(&self, ctx: &JobCtx<'_>, label: &str, tx: TxData) -> Result<B256> {
        if self.eoa.is_none() {
            bail!("no public EOA key configured, cannot send {label} transaction");
        }
        ctx.step(format!("sending {label} transaction from EOA"));
        let pending = self
            .provider
            .send_transaction(tx.into())
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
                let approve = TxData::new(
                    token,
                    ERC20::approveCall {
                        spender,
                        amount: U256::from(value),
                    }
                    .abi_encode()
                    .into(),
                    U256::ZERO,
                );
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
            let builder = TransactionBuilder::new().unshield(signer, to, asset_id, value)?;
            return self.submit(ctx, builder, transport, Vec::new(), None, None).await;
        }

        if token != self.chain.wrapped_base_token {
            bail!("native unshield only applies to the wrapped base token");
        }
        if transport != Transport::Erc4337 {
            // Legacy and direct: the transaction unshields the wrapped token to the RelayAdapt
            // contract, which unwraps its whole balance and forwards the native currency in the
            // same EVM transaction (RelayAdapt.relay). The recipient and the unwrap are bound
            // into the proofs through adaptParams: whoever submits carries bytes it cannot alter.
            let relay_adapt = self.chain.relay_adapt_contract;
            let mut action = RelayAction::unshield_base_token(relay_adapt, to, &mut rand::rng());
            // As the community engine: all or nothing when we send it ourselves; through a
            // broadcaster the Railgun transaction must land even if a call fails, since the
            // fee is paid either way.
            action.require_success = transport == Transport::Direct;
            let builder =
                action.bind(TransactionBuilder::new().unshield(signer, relay_adapt, asset_id, value)?);
            return self.submit(ctx, builder, transport, Vec::new(), None, Some(action)).await;
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
        let builder = TransactionBuilder::new().unshield(signer, sender.address(), asset_id, value)?;
        self.submit(ctx, builder, transport, calls, Some(sender), None).await
    }

    /// Proves and submits a private transaction over the selected transport. `relay` is the
    /// RelayAdapt action `builder` was bound to, if any: the proved transaction is then sent as
    /// `RelayAdapt.relay(...)`.
    async fn submit(
        &mut self,
        ctx: &JobCtx<'_>,
        builder: TransactionBuilder,
        transport: Transport,
        calls: Vec<Call>,
        sender: Option<EoaSigner>,
        relay: Option<RelayAction>,
    ) -> Result<Value> {
        if relay.is_some() && transport == Transport::Erc4337 {
            bail!("RelayAdapt actions are for the legacy and direct transports");
        }
        let result = match transport {
            Transport::Legacy => {
                if !calls.is_empty() {
                    bail!(
                        "the legacy transport runs post-transaction calls through RelayAdapt \
                         (RelayAction), not through a sender account"
                    );
                }
                self.submit_legacy(ctx, builder, relay.as_ref()).await?
            }
            Transport::Direct => {
                if self.eoa.is_none() {
                    bail!("direct transport needs a public EOA key");
                }
                // Exercises the dummy-proof path the legacy transport will use for fee quotes:
                // same calldata shape, zero proof, estimated from the verification bypass origin.
                match self
                    .estimate_with_dummy_proof(builder.clone(), None, relay.as_ref())
                    .await
                {
                    Ok(gas) => ctx.step(format!("dummy-proof gas estimate: {gas}")),
                    Err(e) => ctx.step(format!("dummy-proof gas estimate failed (non fatal): {e:#}")),
                }
                ctx.step("building and proving (direct transport)");
                let mut proved = self.railgun.build(builder, &mut rand::rng()).await?;
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
                    let margin_percent = self
                        .fee_limits
                        .gas_margin_percent
                        .unwrap_or(DEFAULT_GAS_MARGIN_PERCENT)
                        .min(MAX_GAS_MARGIN_PERCENT);
                    match self
                        .simulate_gas_limits(ctx, &builder, &bundler, &account, &calls, margin_percent)
                        .await
                    {
                        Ok(gas) => {
                            ctx.step(format!(
                                "proving once from ephemeral sender {}",
                                sender.address()
                            ));
                            let (signable, report) = self
                                .railgun
                                .prepare_userop_single_proof(
                                    builder,
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
                                "gas limits could not be simulated and falling back to several \
                                 signatures is disabled. Nothing was signed",
                            ));
                        }
                        Err(e) => {
                            ctx.step(format!(
                                "gas limits could not be simulated ({e:#}): falling back to the iterative estimate, which signs several times"
                            ));
                            let signable = self
                                .railgun
                                .prepare_userop(
                                    builder,
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
                            builder,
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
        builder: TransactionBuilder,
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

        let signer = self.signer.clone() as Arc<dyn RailgunSigner>;
        let with_fee = |fee: u128| -> Result<TransactionBuilder> {
            Ok(builder
                .clone()
                .broadcaster_fee(signer.clone(), broadcaster_address, fee_asset, fee)?
                .min_gas_price(gas_price))
        };
        let fee_for = |gas: u64| {
            token_fee(quote.fee_per_unit_gas, gas, gas_price)
                .ok_or_else(|| anyhow!("fee overflow"))
        };

        // The fee changes note values, possibly the number of inputs, hence the gas: estimate
        // twice, the second time with a fee of the right magnitude.
        let mut fee = fee_for(FIRST_GUESS_GAS)?;
        let mut gas = 0;
        for round in 1..=2 {
            gas = self
                .estimate_with_dummy_proof(with_fee(fee)?, Some(gas_price), relay)
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

        ctx.step("building and proving (legacy transport)");
        let mut proved = self.railgun.build(with_fee(fee)?, &mut rand::rng()).await?;
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
        let sealed = self.broadcaster.seal(
            BroadcastRequest {
                quote: quote.clone(),
                to: proved.tx_data.to.to_checksum(None),
                calldata: proved.tx_data.data.to_vec(),
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

    /// Gas limits of the UserOperation, measured before any proof or signature.
    ///
    /// A dummy-proof UserOperation is run through `userop_kit::validation_probe`: an `eth_call`
    /// from the Railgun verification bypass origin, with the probe's code overriding the
    /// EntryPoint, executes account validation, paymaster validation (which performs the
    /// `transact`), the tail calls and `postOp`, and reports the gas of each. Two rounds, since
    /// the fee is an output note and can change the inputs selected. Only `preVerificationGas`
    /// is computed, by the reference formula: it prices inclusion, not execution, and gets twice
    /// the margin because the bundler has the last word on it.
    async fn simulate_gas_limits(
        &mut self,
        ctx: &JobCtx<'_>,
        builder: &TransactionBuilder,
        bundler: &PimlicoBundler,
        account: &SimpleSmartAccount,
        calls: &Vec<Call>,
        margin_percent: u32,
    ) -> Result<UserOperationGasEstimate> {
        use alloy::rpc::types::state::{AccountOverride, StateOverride};
        use userop_kit::validation_probe::{self as probe, alto::AltoPolicy, pad_gas};

        let price = (bundler as &dyn Bundler)
            .gas_price()
            .await
            .context("asking the bundler for its gas price")?
            .ok_or_else(|| anyhow!("the bundler gives no gas price outside of a simulation"))?;
        let railgun_signer = self.signer.clone() as Arc<dyn RailgunSigner>;
        let fee_token = self.chain.wrapped_base_token;

        // The probe does not enforce limits: they only enter through `maxCost`, against which
        // the paymaster checks the fee. Each round therefore pays exactly its own limits, as the
        // real UserOperation will. Round one starts from plausible figures so that this fee is
        // affordable; round two uses what round one measured.
        let mut gas = UserOperationGasEstimate {
            pre_verification_gas: 150_000,
            verification_gas_limit: 100_000,
            call_gas_limit: if calls.is_empty() { 20_000 } else { 150_000 },
            paymaster_verification_gas_limit: Some(1_200_000),
            paymaster_post_op_gas_limit: Some(50_000),
            max_fee_per_gas: price.max_fee_per_gas,
            max_priority_fee_per_gas: price.max_priority_fee_per_gas,
        };
        let total_of = |g: &UserOperationGasEstimate| {
            g.pre_verification_gas
                + g.verification_gas_limit
                + g.call_gas_limit
                + g.paymaster_verification_gas_limit.unwrap_or(0)
                + g.paymaster_post_op_gas_limit.unwrap_or(0)
        };
        let mut measured = None;
        for _round in 1..=2 {
            let fee = total_of(&gas) * price.max_fee_per_gas;
            let dummy = self
                .railgun
                .dummy_userop(
                    builder.clone(),
                    account,
                    railgun_signer.clone(),
                    fee_token,
                    calls,
                    fee,
                    gas,
                    &mut rand::rng(),
                )
                .await?;
            let max_cost = U256::from(dummy.total_gas_limit()) * U256::from(price.max_fee_per_gas);
            let request = probe::request(&dummy, max_cost);

            let mut overrides = StateOverride::default();
            for (address, code) in &request.code_overrides {
                overrides.insert(*address, AccountOverride::default().with_code(code.clone()));
            }
            if let Some((from, to)) = request.copy_code_from {
                // The 7702 delegation of the fresh sender is not on-chain yet: give it the code
                // it will delegate to.
                let code = self
                    .provider
                    .get_code_at(from)
                    .await
                    .context("reading the account implementation code")?;
                overrides.insert(to, AccountOverride::default().with_code(code));
            }
            overrides.insert(
                railgun::provider::VERIFICATION_BYPASS,
                AccountOverride::default().with_balance(U256::from(10u128.pow(24))),
            );

            let call = alloy::rpc::types::TransactionRequest::default()
                .from(railgun::provider::VERIFICATION_BYPASS)
                .to(request.to)
                .input(request.data.clone().into())
                .gas_limit(25_000_000);
            let answer = self
                .provider
                .call(call)
                .overrides(overrides)
                .await
                .context("eth_call with state overrides (does this RPC support them?)")?;

            let phases = probe::decode(&answer, !calls.is_empty())?;

            // The bundler prices paymaster data and signature as all non-zero bytes: take its
            // way of counting, which is the higher one, and the usual margin.
            let pre_verification =
                probe::pre_verification_gas(&dummy).max(probe::alto::pre_verification_gas(&dummy));
            gas.pre_verification_gas = pad_gas(pre_verification, margin_percent, GAS_BUCKET);
            // The EntryPoint charges its own pre-validation work to this limit (AA26), which the
            // probe cannot see: see `ENTRY_POINT_VALIDATION_OVERHEAD`.
            gas.verification_gas_limit = pad_gas(
                phases.account_validation + probe::ENTRY_POINT_VALIDATION_OVERHEAD,
                margin_percent,
                GAS_BUCKET,
            );
            // Execution calls. The probe's search gives the smallest limit that works on-chain;
            // the bundler will ask for more, by a rule read in its source and reproduced in
            // `validation_probe::alto`: its own search ladder, then a multiplier (25.3k needed
            // gives 68,136, to the unit). Size the limit from that prediction, with half the
            // margin on top in case the deployment's multiplier moves a little. This is the one
            // limit whose shortfall would strand funds on the ephemeral sender.
            gas.call_gas_limit = if calls.is_empty() {
                30_000
            } else {
                pad_gas(
                    AltoPolicy::PIMLICO_PUBLIC.call_gas_limit(phases.call_limit),
                    margin_percent / 2,
                    GAS_BUCKET,
                )
            };
            gas.paymaster_verification_gas_limit =
                Some(pad_gas(phases.paymaster_validation, margin_percent, GAS_BUCKET));
            gas.paymaster_post_op_gas_limit = Some(if phases.post_op_called {
                pad_gas(phases.post_op, margin_percent, GAS_BUCKET).max(10_000)
            } else {
                10_000
            });
            let total = total_of(&gas);
            measured = Some((phases, pre_verification, total));
        }

        let (phases, pre_verification, total) = measured.expect("two rounds ran");
        ctx.step(format!(
            "simulated before signing: account {}, paymaster {}, calls {} used / {} minimal limit, post-op {}, pre-verification {} (bundler's formula); limits with {margin_percent}% margin total {total} gas",
            phases.account_validation, phases.paymaster_validation, phases.call, phases.call_limit, phases.post_op, pre_verification
        ));
        Ok(gas)
    }

    /// `gas_price` must be the `minGasPrice` bound in the builder, if any: the Railgun contract
    /// compares it to `tx.gasprice`, which is 0 in an estimate that names no price, and reverts
    /// with "Gas price too low". The estimate then runs as the legacy (type 0) transaction the
    /// broadcaster will send.
    async fn estimate_with_dummy_proof(
        &mut self,
        builder: TransactionBuilder,
        gas_price: Option<u128>,
        relay: Option<&RelayAction>,
    ) -> Result<u64> {
        let mut dummy = self.railgun.build_dummy(builder, &mut rand::rng()).await?;
        if let Some(action) = relay {
            action.package(&mut dummy);
        }
        let mut request: alloy::rpc::types::TransactionRequest = dummy.tx_data.into();
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

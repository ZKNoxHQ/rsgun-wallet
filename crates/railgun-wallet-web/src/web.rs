//! The wallet daemon's engine and `/api/…` routes, for the browser. Same paths and JSON bodies as
//! `railgun-wallet/src/ipc.rs`, so `railgun-wallet/static/index.html` runs unchanged.
//!
//! One actor task drives the `RailgunProvider` (commands run in order, as on the daemon's engine
//! thread); GET endpoints read the shared snapshot without waiting. The SDK database is IndexedDB.
//!
//! Stage 2a: `/api/unlock` (mnemonic or raw keys), `/api/op` `sync`, the job/step model and the
//! status snapshot with balances. Ledger unlock needs a main-thread APDU bridge (stage 2b);
//! shield/transfer/unshield/4337/legacy are stage 3; the Waku bridge is stage 4.

use std::{cell::RefCell, collections::HashMap, str::FromStr, sync::Arc};

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
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use kohaku_kv_store::Store;
use kv_store_js::JsKvStore;
use railgun::{
    account::{
        address::RailgunAddress,
        chain::ChainId as RgChainId,
        signer::{PrivateKeySigner as RgSigner, RailgunSigner},
    },
    builder::RailgunBuilder,
    caip::AssetId,
    chain_config::ChainConfig,
    provider::{RailgunProvider, RailgunProviderError},
    transact::TransactionBuilder,
};
use railgun_broadcaster::{
    BroadcastRequest, BroadcasterClient, ClientError, FeeQuote, NoQuote, NwakuRest, WakuTransport,
    PAR_RATE_WRAPPED_BASE_TOKEN, RelayAction, fees::now_ms as quote_now_ms, token_fee,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use userop_kit::{
    bundler::{Bundler, pimlico::PimlicoBundler},
    smart_account::simple_smart_account::{Call, SimpleSmartAccount},
};
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::spawn_local;

use crate::{
    keys::{self, Derivation},
    shared::{self, JobState, LegacyStatus, SharedRef, StatusSnapshot, TransportInfo, now_ms},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

sol! {
    #[sol(rpc)]
    contract ERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }

    contract WETH9 {
        function withdraw(uint256 wad) external;
    }
}

/// Post-transaction wait before the automatic sync, as on the daemon.
const POST_TX_SYNC_DELAY_MS: u32 = 20_000;

/// Legacy transport, as on the daemon.
const DEFAULT_MAX_FEE_RATE: &str = "1.5";
const BROADCASTER_DRAW_PERCENT: u32 = 10;
const SILENT_BROADCASTER_PENALTY_MS: u64 = 600_000;
const FIRST_GUESS_GAS: u64 = 700_000;

#[wasm_bindgen(module = "/js/idb.js")]
extern "C" {
    #[wasm_bindgen(js_name = openDb)]
    fn open_db(name: &str) -> JsValue;
    #[wasm_bindgen(js_name = deleteDb, catch)]
    async fn delete_db(name: &str) -> Result<JsValue, JsValue>;
}

// ---------------------------------------------------------------------------- request shapes

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct UnlockParams {
    mnemonic: Option<String>,
    spending_key: Option<String>,
    viewing_key: Option<String>,
    derivation: Option<String>,
    index: u32,
    chain_id: u64,
    ledger: bool,
    ledger_transport: Option<String>,
    rpc_url: Option<String>,
    eoa_key: Option<String>,
    /// The public account is the Ledger's Ethereum app (`m/44'/60'/0'/0/index`).
    eoa_ledger: bool,
    poi: bool,
    bundler_url: Option<String>,
    // Stage 4 (Waku): parsed but unused for now.
    waku_url: Option<String>,
    waku_mode: Option<String>,
    trusted_fee_signers: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Transport {
    /// ERC-4337 UserOperation through the kohaku privacy paymaster.
    Erc4337,
    /// Railgun community broadcasters (Waku). Needs the page's Waku bridge (stage 4).
    Legacy,
    /// Self-broadcast from the public EOA. No sender privacy, debug only.
    Direct,
}

/// Fee limits, exactly the daemon's `engine::FeeLimits` shape.
#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct FeeLimits {
    max_fee_rate: Option<String>,
    max_fee: Option<String>,
    #[serde(default)]
    single_proof: bool,
    gas_margin_percent: Option<u32>,
    broadcaster: Option<String>,
    #[serde(default)]
    single_proof_strict: bool,
}

const DEFAULT_GAS_MARGIN_PERCENT: u32 = 25;
const MAX_GAS_MARGIN_PERCENT: u32 = 200;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Op {
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
        #[serde(default)]
        native: bool,
        transport: Transport,
        #[serde(default, flatten)]
        fee_limits: FeeLimits,
    },
}

impl Op {
    fn kind(&self) -> &'static str {
        match self {
            Op::Sync => "sync",
            Op::Shield { .. } => "shield",
            Op::Transfer { .. } => "transfer",
            Op::Unshield { .. } => "unshield",
        }
    }
}

// ---------------------------------------------------------------------------- actor

enum Command {
    Unlock(Box<UnlockParams>, oneshot::Sender<Result<()>>),
    Job { id: u64, op: Op },
    Lock(oneshot::Sender<()>),
    /// Deletes the IndexedDB database of the open wallet and locks it.
    EmptyCache(oneshot::Sender<Result<String>>),
}

struct Session {
    chain: ChainConfig,
    provider: DynProvider,
    eoa: Option<EoaSigner>,
    /// Set when the public account is the Ledger's Ethereum app (then `eoa` is `None`).
    eoa_ledger: Option<LedgerEoa>,
    /// The spending signer is the Ledger over USB: private operations first bring its Railgun
    /// app to the front.
    ledger_usb: bool,
    /// Account index of a Ledger signer, whose cached keys "Empty cache" also forgets.
    ledger_index: Option<u32>,
    /// The wallet's IndexedDB database, also holding the Ledger Ethereum address once read.
    db: Store,
    eoa_source: Option<String>,
    signer: Arc<dyn RailgunSigner>,
    railgun: RailgunProvider,
    derivation: &'static str,
    bundler_url: String,
    tokens: HashMap<Address, (String, u8)>,
    /// IndexedDB database of this wallet, for "Empty cache".
    db_name: String,
    broadcaster: Arc<BroadcasterClient>,
    /// The tab's Waku link when that is the transport, to read what happened to publishes.
    bridge: Option<Arc<railgun_broadcaster::BrowserBridge>>,
    /// Broadcasters that did not answer, with the time of the failure. Skipped for a while.
    silent_broadcasters: Vec<(String, web_time::Instant)>,
    /// Cleared on lock/re-unlock so the fee-monitor task of this session stops.
    monitor_alive: Arc<std::sync::atomic::AtomicBool>,
    /// Gas limits the bundler asked for after a single proof, kept for the session so the next
    /// operation fixes them before proving (one signature). See `prove_once_learning`.
    learned_gas: LearnedGas,
    /// Transaction that created each note, by (tree, leaf). A position never changes owner, so
    /// each is looked up once. See `resolve_note_txs`.
    note_tx: HashMap<(u32, u32), String>,
}

/// Floors learned from the bundler's post-proof estimates, per gas field.
#[derive(Default, Clone, Copy)]
struct LearnedGas {
    pre_verification: u128,
    verification: u128,
    call: u128,
    paymaster_verification: u128,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.monitor_alive
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

struct Actor {
    shared: SharedRef,
    session: Option<Session>,
}

thread_local! {
    static SHARED: SharedRef = Arc::new(shared::Shared::default());
    static TX: RefCell<Option<mpsc::UnboundedSender<Command>>> = const { RefCell::new(None) };
}

fn shared_ref() -> SharedRef {
    SHARED.with(Clone::clone)
}

fn send(cmd: Command) -> Result<()> {
    TX.with(|t| {
        t.borrow()
            .as_ref()
            .ok_or_else(|| anyhow!("engine not started"))?
            .unbounded_send(cmd)
            .map_err(|_| anyhow!("engine task is gone"))
    })
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    // As on the daemon: `FrontLogLayer` mirrors info+ events of our crates into `Shared.logs`,
    // which `/api/logs` serves to the front's log panel (the layer itself filters to railgun*/
    // userop_kit/waku_light targets). The browser console only gets warnings.
    {
        use tracing_subscriber::{Layer as _, layer::SubscriberExt, util::SubscriberInitExt};
        if let Err(e) = tracing_subscriber::registry()
            .with(
                crate::shared::FrontLogLayer(shared_ref())
                    .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
            )
            // The console layer must not use WASMLayer's own max_level: its `enabled()` would
            // veto info events globally, starving FrontLogLayer. A per-layer filter only
            // affects this layer.
            .with(
                tracing_wasm::WASMLayer::new(
                    tracing_wasm::WASMLayerConfigBuilder::new()
                        .set_max_level(tracing::Level::TRACE)
                        .build(),
                )
                .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
            )
            .try_init()
        {
            web_sys::console::error_1(&format!("tracing init failed: {e}").into());
        }
        // Probe: this line must appear in the front's log panel as soon as the page loads.
        tracing::info!(target: "railgun_wallet_web", "engine log online (v{VERSION})");
    }
    let (tx, mut rx) = mpsc::unbounded::<Command>();
    TX.with(|t| *t.borrow_mut() = Some(tx));
    spawn_local(async move {
        let mut actor = Actor { shared: shared_ref(), session: None };
        // Commands that arrived while a sync ran, in order.
        let mut queued = std::collections::VecDeque::new();
        loop {
            let cmd = match queued.pop_front() {
                Some(cmd) => cmd,
                None => match rx.next().await {
                    Some(cmd) => cmd,
                    None => break,
                },
            };
            let Command::Job { id, op: Op::Sync } = cmd else {
                actor.handle(cmd).await;
                continue;
            };
            // A sync can run for long (the first copy of a POI list): a lock stops it, as a
            // reload would. The SDK saves in atomic steps, so the next unlock resumes from the
            // last one. Transactions are never interrupted: a lock waits for them.
            let mut lock = None;
            {
                let job = futures::FutureExt::fuse(actor.handle(Command::Job { id, op: Op::Sync }));
                futures::pin_mut!(job);
                loop {
                    futures::select! {
                        () = job => break,
                        next = rx.next() => match next {
                            Some(Command::Lock(reply)) => {
                                lock = Some(reply);
                                break;
                            }
                            Some(other) => queued.push_back(other),
                            None => {
                                job.await;
                                break;
                            }
                        },
                    }
                }
            }
            if let Some(reply) = lock {
                // Operations queued behind the sync belonged to the session that is closing.
                let dropped: Vec<u64> = queued
                    .iter()
                    .filter_map(|cmd| match cmd {
                        Command::Job { id, .. } => Some(*id),
                        _ => None,
                    })
                    .collect();
                queued.retain(|cmd| !matches!(cmd, Command::Job { .. }));
                if let Ok(mut jobs) = actor.shared.jobs.lock() {
                    for (job_id, error) in std::iter::once((id, "stopped: the wallet was locked"))
                        .chain(dropped.into_iter().map(|d| (d, "not run: the wallet was locked")))
                    {
                        jobs.update(job_id, |job| {
                            job.finished_at = Some(now_ms());
                            job.state = JobState::Failed;
                            job.error = Some(error.into());
                        });
                    }
                }
                actor.handle(Command::Lock(reply)).await;
            }
        }
    });
}

fn push_step(shared: &SharedRef, id: u64, msg: String) {
    if let Ok(mut jobs) = shared.jobs.lock() {
        jobs.update(id, |job| job.steps.push(msg));
    }
}

impl Actor {
    async fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Unlock(p, reply) => {
                let r = self.unlock(*p).await;
                if r.is_ok() {
                    self.refresh_status().await;
                }
                let _ = reply.send(r);
            }
            Command::Job { id, op } => {
                if let Ok(mut jobs) = self.shared.jobs.lock() {
                    jobs.update(id, |job| job.state = JobState::Running);
                }
                let result = self.run_op(id, op).await;
                if let Ok(mut jobs) = self.shared.jobs.lock() {
                    jobs.update(id, |job| {
                        job.finished_at = Some(now_ms());
                        match &result {
                            Ok(v) => {
                                job.state = JobState::Done;
                                job.result = Some(v.clone());
                            }
                            Err(e) => {
                                job.state = JobState::Failed;
                                job.error = Some(format!("{e:#}"));
                            }
                        }
                    });
                }
                self.refresh_status().await;
            }
            Command::EmptyCache(reply) => {
                // Queued behind any running job, like on the daemon: never deletes under a sync
                // or a proof. Dropping the session releases the SDK's database handle.
                let result = match self.session.take() {
                    None => Err(anyhow!("open the wallet first: the cache is per wallet")),
                    Some(session) => {
                        let name = session.db_name.clone();
                        let ledger_index = session.ledger_index;
                        drop(session);
                        if let Some(index) = ledger_index {
                            forget_ledger_keys(index).await;
                        }
                        match delete_db(&name).await {
                            Ok(_) => Ok(format!(
                                "IndexedDB database {name}{}",
                                if ledger_index.is_some() { " and the Ledger keys" } else { "" }
                            )),
                            Err(e) => Err(anyhow!("deleting {name}: {e:?}")),
                        }
                    }
                };
                if let Ok(mut s) = self.shared.status.write() {
                    *s = StatusSnapshot::default();
                    s.updated_at = now_ms();
                }
                if let Ok(mut l) = self.shared.legacy.write() {
                    *l = LegacyStatus::default();
                }
                let _ = reply.send(result);
            }
            Command::Lock(reply) => {
                self.session = None;
                if let Ok(mut s) = self.shared.status.write() {
                    *s = StatusSnapshot::default();
                    s.updated_at = now_ms();
                }
                if let Ok(mut l) = self.shared.legacy.write() {
                    *l = LegacyStatus::default();
                }
                let _ = reply.send(());
            }
        }
    }

    async fn unlock(&mut self, p: UnlockParams) -> Result<()> {
        let mut chain = railgun_chains::from_chain_id(p.chain_id)
            .ok_or_else(|| anyhow!("unsupported chain id {}", p.chain_id))?;

        // ppoi.fdi.network publishes an IPv6 address that refuses connections: IPv6-only networks
        // (typically phones) cannot reach it directly from the browser. When the host serves a
        // same-origin /poi relay (Netlify `_redirects`, serve.py), prefer it — it reaches the POI
        // node over IPv4 server-side. Hosts without a relay (GitHub Pages) keep the direct URL.
        if p.poi {
            if let Some(relay) = poi_relay().await {
                tracing::info!("POI through the same-origin relay {relay}");
                chain.poi_endpoint = relay;
            }
        }

        let ledger_usb = p.ledger && p.ledger_transport.as_deref().unwrap_or("usb") == "usb";
        // Chain-agnostic address (RgChainId::All), as Railway displays it.
        // The Ledger's Ethereum address, when the cache or this unlock knows it.
        let mut ledger_eth: Option<Address> = None;
        let (signer, derivation): (Arc<dyn RailgunSigner>, &'static str) = if p.ledger {
            let usb = match p.ledger_transport.as_deref().unwrap_or("usb") {
                "usb" => true,
                "ble" | "bluetooth" => false,
                other => bail!("unknown Ledger transport {other:?} (use \"usb\" or \"ble\")"),
            };
            let cached = read_ledger_keys(p.index).await;
            let cached_keys = cached.as_ref().and_then(LedgerKeys::keys);
            let signer: Arc<dyn RailgunSigner> = match (usb, cached_keys) {
                // From the cache: nothing to approve, any app may be open. The device is only
                // needed to sign, and the Railgun app is brought to the front then.
                (true, Some((spending_pubkey, viewing_key))) => {
                    // The page granted USB access during the unlock click (shim.js); in this
                    // worker getDevices() then hands the device over without a picker.
                    let device = railgun_ledger::transport::webusb::WebUsbLedger::connect_existing()
                        .await
                        .map_err(|e| anyhow!("Ledger: {e} (plugged in and unlocked?)"))?;
                    railgun_ledger::LedgerSigner::from_cached(
                        device,
                        RgChainId::All,
                        p.index,
                        spending_pubkey,
                        viewing_key,
                    )
                }
                (false, Some((spending_pubkey, viewing_key))) => {
                    let device = crate::ble_bridge::BridgedBleLedger::connect()
                        .await
                        .map_err(|e| anyhow!("Ledger over BLE: {e}"))?;
                    railgun_ledger::LedgerSigner::from_cached(
                        device,
                        RgChainId::All,
                        p.index,
                        spending_pubkey,
                        viewing_key,
                    )
                }
                // From the device: its approvals, the Railgun app opened first over USB.
                (true, None) => {
                    let device = switch_app(
                        &|msg| tracing::info!("{msg}"),
                        railgun_ledger::eth::RAILGUN_APP_NAME,
                        APP_SWITCH_WAIT_MS,
                    )
                    .await
                    .map_err(|e| anyhow!("Ledger: {e:#} (plugged in and unlocked?)"))?;
                    railgun_ledger::LedgerSigner::connect(device, RgChainId::All, p.index)
                        .await
                        .map_err(|e| anyhow!("Ledger Railgun app: {e}"))?
                }
                (false, None) => {
                    // The page owns the GATT session (Web Bluetooth is main-thread-only);
                    // shim.js connected it during the unlock click. This speaks the Ledger
                    // framing over the page bridge.
                    let device = crate::ble_bridge::BridgedBleLedger::connect()
                        .await
                        .map_err(|e| anyhow!("Ledger over BLE: {e}"))?;
                    railgun_ledger::LedgerSigner::connect(device, RgChainId::All, p.index)
                        .await
                        .map_err(|e| anyhow!("Ledger Railgun app: {e}"))?
                }
            };
            if cached_keys.is_some() {
                tracing::info!("Ledger account {}: keys from this browser's cache", p.index);
            }
            ledger_eth = cached.as_ref().and_then(|k| k.eth_address(p.index));
            // The Ethereum address, read once without a prompt: over USB the wallet switches to
            // the Ethereum app and back. Best effort: without it, the first shield reads it.
            if usb && p.eoa_ledger && ledger_eth.is_none() {
                match read_ledger_eth_address(p.index).await {
                    Ok(address) => ledger_eth = Some(address),
                    Err(e) => tracing::warn!("Ledger Ethereum address not read at unlock: {e:#}"),
                }
            }
            let keys = LedgerKeys::new(
                signer.spending_public_key(),
                &signer.viewing_key(),
                p.index,
                ledger_eth,
            );
            if cached.as_ref() != Some(&keys) {
                write_ledger_keys(p.index, &keys).await;
            }
            (signer, "ledger")
        } else {
            let (rgkeys, derivation): (keys::RailgunKeys, &'static str) =
                match (&p.mnemonic, &p.spending_key, &p.viewing_key) {
                    (Some(m), _, _) if !m.trim().is_empty() => {
                        let kohaku = p.derivation.as_deref() == Some("kohaku");
                        let scheme = if kohaku { Derivation::Kohaku } else { Derivation::Railgun };
                        (
                            keys::derive(m, p.index, scheme)?,
                            if kohaku { "kohaku" } else { "railgun" },
                        )
                    }
                    (_, Some(s), Some(v)) => (keys::from_hex(s, v)?, "raw"),
                    _ => bail!(
                        "provide a mnemonic, both spending and viewing keys, or a Ledger device"
                    ),
                };
            (
                RgSigner::new(rgkeys.spending, rgkeys.viewing, RgChainId::All),
                derivation,
            )
        };

        // Public EOA (for shielding and the direct transport): an explicit key wins, else the
        // Ethereum account of the same phrase at the same index.
        let eoa_key = p.eoa_key.as_deref().map(str::trim).filter(|k| !k.is_empty());
        let (eoa, eoa_source) = match (eoa_key, p.mnemonic.as_deref()) {
            // The Ledger's Ethereum app: no key on the host, the address is read at the first
            // shield (the Railgun app is the one open now).
            _ if p.eoa_ledger => (
                None,
                Some(format!("Ledger Ethereum app ({})", railgun_ledger::eth::eth_path(p.index))),
            ),
            (Some(k), _) => (
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
        if !(rpc_url.starts_with("http://") || rpc_url.starts_with("https://")) {
            bail!("RPC URL must start with http(s)://, got \"{rpc_url}\"");
        }

        let provider = match &eoa {
            Some(e) => ProviderBuilder::new()
                .network::<Ethereum>()
                .wallet(e.clone())
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
        let rpc_chain = provider
            .get_chain_id()
            .await
            .context("eth_chainId (the RPC must allow requests from this page: CORS)")?;
        if rpc_chain != chain.id {
            bail!("RPC is on chain {rpc_chain}, expected {}", chain.id);
        }

        // One IndexedDB database per account and chain, keyed by the master public key.
        let master_key = signer.address().master_key().to_string();
        let tag = hex::encode(&Sha256::digest(master_key.as_bytes())[..8]);
        let name = format!("rgw-{}-{tag}", chain.id);
        let db = JsKvStore::store(open_db(&name).unchecked_into());
        let eoa_ledger = if p.eoa_ledger {
            Some(LedgerEoa { index: p.index, address: ledger_eth })
        } else {
            None
        };

        let mut builder =
            RailgunBuilder::new(chain.clone(), provider.clone()).with_database(db.clone());
        if p.poi {
            builder = builder.with_poi();
        }
        let mut railgun = builder.build().await.context("building railgun provider")?;
        railgun.register(signer.clone()).await?;

        let bundler_url = p
            .bundler_url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| default_bundler_url(chain.id));

        // Legacy transport: broadcasters over Waku. "native" is the Rust light node running in
        // this worker (over the browser's WebSocket); "browser" is js-waku in the wallet tab,
        // bridged through /api/waku/exchange; "nwaku" a local node over REST (https pages cannot
        // reach a plain-http local node: browser mixed-content rule).
        let waku_url = p
            .waku_url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_default();
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
            None | Some("") | Some("native") => "native",
            Some("browser") => "browser",
            Some("nwaku") => "nwaku",
            Some(other) => bail!("unknown Waku mode \"{other}\" (expected native, browser or nwaku)"),
        };
        let transport: Arc<dyn WakuTransport> = match waku_mode {
            "browser" => self.shared.bridge.clone(),
            "nwaku" => Arc::new(NwakuRest::new(waku_url.clone())),
            _ => Arc::new(railgun_broadcaster::LightNodeTransport::for_chain(chain.id)),
        };
        let broadcaster = Arc::new(if trusted_signers.is_empty() {
            BroadcasterClient::new(transport, chain.id)
        } else {
            BroadcasterClient::with_trusted_signers(transport, chain.id, &trusted_signers)
                .map_err(|e| anyhow!("{e}"))?
        });
        // Replaces the monitor of a previous session, if any (its Drop stops the old task).
        self.session = None;
        let monitor_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        spawn_local(fee_monitor(
            broadcaster.clone(),
            self.shared.clone(),
            waku_mode,
            waku_url,
            monitor_alive.clone(),
        ));

        tracing::info!(
            "unlocked {derivation} · {} · chain {} · POI {}",
            signer.address(),
            chain.id,
            if p.poi { "on" } else { "off" }
        );
        self.session = Some(Session {
            chain,
            provider,
            eoa,
            eoa_source,
            eoa_ledger,
            ledger_usb,
            ledger_index: p.ledger.then_some(p.index),
            db,
            signer,
            railgun,
            derivation,
            bundler_url,
            tokens: HashMap::new(),
            db_name: name,
            broadcaster,
            bridge: (waku_mode == "browser").then(|| self.shared.bridge.clone()),
            silent_broadcasters: Vec::new(),
            monitor_alive,
            learned_gas: LearnedGas::default(),
            note_tx: HashMap::new(),
        });
        Ok(())
    }

    async fn run_op(&mut self, id: u64, op: Op) -> Result<Value> {
        let shared = self.shared.clone();
        let s = self
            .session
            .as_mut()
            .ok_or_else(|| anyhow!("wallet is locked"))?;
        match op {
            Op::Sync => {
                push_step(&shared, id, "syncing UTXO tree, TXID tree and pending POI proofs".into());
                s.railgun.sync().await?;
                Ok(json!({ "syncedBlock": s.railgun.synced_block() }))
            }
            Op::Shield { asset, amount } => shield(s, &shared, id, &asset, &amount).await,
            Op::Transfer { to, asset, amount, memo, transport, fee_limits } => {
                let to = RailgunAddress::from_str(to.trim())
                    .map_err(|e| anyhow!("invalid 0zk recipient: {e}"))?;
                let (asset, value, _) = resolve_amount(s, &asset, &amount).await?;
                let builder = TransactionBuilder::new().transfer(
                    s.signer.clone() as Arc<dyn RailgunSigner>,
                    to,
                    asset,
                    value,
                    &memo,
                );
                submit(s, &shared, id, builder, transport, Vec::new(), None, None, &fee_limits).await
            }
            Op::Unshield { to, asset, amount, native, transport, fee_limits } => {
                let to = Address::from_str(to.trim())
                    .map_err(|e| anyhow!("invalid 0x recipient: {e}"))?;
                unshield(s, &shared, id, to, &asset, &amount, native, transport, &fee_limits).await
            }
        }
    }

    async fn refresh_status(&mut self) {
        if let Some(snap) = self.snapshot().await {
            if let Ok(mut s) = self.shared.status.write() {
                *s = snap;
            }
        }
    }

    async fn snapshot(&mut self) -> Option<StatusSnapshot> {
        let session = self.session.as_mut()?;
        let address = session.signer.address();

        let entries = session.railgun.balance(address.clone()).await;
        let mut balances = Vec::new();
        for entry in entries {
            let mut view = json!({
                "asset": entry.asset.to_string(),
                "amount": entry.amount.to_string(),
                "poiStatus": entry.poi_status,
            });
            if let AssetId::Erc20(token) = entry.asset {
                let (symbol, decimals) = token_meta(session, token).await;
                view["token"] = json!(token.to_string());
                view["symbol"] = json!(symbol);
                view["decimals"] = json!(decimals);
                view["formatted"] = json!(trim_decimal(
                    format_units(U256::from(entry.amount), decimals).unwrap_or_default()
                ));
            }
            balances.push(view);
        }
        balances.sort_by_key(|v| v["asset"].as_str().map(str::to_owned));

        let entries = session.railgun.notes(address.clone()).await;
        let positions: Vec<(u32, u32)> =
            entries.iter().map(|n| (n.tree_number, n.leaf_index)).collect();
        resolve_note_txs(session, &positions).await;
        // Token symbols, read from each token contract through the session's RPC (any chain);
        // cached per token by `token_meta`.
        let mut symbols: HashMap<Address, String> = HashMap::new();
        for n in &entries {
            if let AssetId::Erc20(token) = n.asset {
                if !symbols.contains_key(&token) {
                    let (symbol, _) = token_meta(session, token).await;
                    symbols.insert(token, symbol);
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
                    "txHash": session.note_tx.get(&(n.tree_number, n.leaf_index)),
                })
            })
            .collect();

        let eoa = eoa_address(session);
        let eoa_balance = match eoa {
            Some(a) => session
                .provider
                .get_balance(a)
                .await
                .ok()
                .and_then(|b| format_units(b, 18u8).ok())
                .map(trim_decimal),
            None => None,
        };

        let has_paymaster =
            session.chain.privacy_paymaster.is_some() && session.chain.railgun_fee_adapter.is_some();
        let transports = vec![
            TransportInfo {
                id: "erc4337",
                label: "4337",
                enabled: has_paymaster,
                note: if has_paymaster {
                    "UserOperation via privacy paymaster, fee paid in shielded wrapped base token".into()
                } else {
                    "no privacy paymaster deployed on this chain".into()
                },
            },
            TransportInfo {
                id: "legacy",
                label: "legacy",
                enabled: false,
                note: "Railgun community broadcasters over Waku, fee paid in shielded wrapped base token".into(),
            },
            TransportInfo {
                id: "direct",
                label: "direct (debug)",
                enabled: eoa.is_some() || session.eoa_ledger.is_some(),
                note: "self-broadcast from the public EOA, links the EOA to the transaction".into(),
            },
        ];

        Some(StatusSnapshot {
            unlocked: true,
            chain_id: Some(session.chain.id),
            address: Some(address.to_string()),
            derivation: Some(session.derivation.to_string()),
            eoa: eoa.map(|a| a.to_string()),
            eoa_source: session.eoa_source.clone(),
            eoa_balance,
            eoa_ledger: session.eoa_ledger.is_some(),
            poi: session.railgun.poi_enabled(),
            poi_list_keys: session.railgun.poi_list_keys(),
            synced_block: Some(session.railgun.synced_block()),
            wrapped_base_token: Some(session.chain.wrapped_base_token.to_string()),
            railgun_smart_wallet: Some(session.chain.railgun_smart_wallet.to_string()),
            bundler_url: Some(session.bundler_url.clone()),
            transports,
            balances,
            notes,
            poi_pending: session
                .railgun
                .poi_pending()
                .into_iter()
                .filter_map(|p| serde_json::to_value(p).ok())
                .collect(),
            updated_at: now_ms(),
        })
    }
}

async fn token_meta(session: &mut Session, token: Address) -> (String, u8) {
    if let Some(meta) = session.tokens.get(&token) {
        return meta.clone();
    }
    let erc20 = ERC20::new(token, session.provider.clone());
    let decimals = erc20.decimals().call().await;
    let symbol = erc20.symbol().call().await;
    let complete = decimals.is_ok() && symbol.is_ok();
    let meta = (symbol.unwrap_or_else(|_| "?".into()), decimals.unwrap_or(18));
    if complete {
        session.tokens.insert(token, meta.clone());
    }
    meta
}

/// Fills `session.note_tx` with the transaction that created each of `positions`, from the
/// subsquid indexer (`commitments` by tree and position). One query per tree for the positions
/// not cached yet. Best effort: on any failure the notes simply show no transaction.
async fn resolve_note_txs(session: &mut Session, positions: &[(u32, u32)]) {
    let mut by_tree: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(tree, leaf) in positions {
        if !session.note_tx.contains_key(&(tree, leaf)) {
            by_tree.entry(tree).or_default().push(leaf);
        }
    }
    for (tree, leaves) in by_tree {
        let query = format!(
            "{{ commitments(where: {{treeNumber_eq: {tree}, treePosition_in: {leaves:?}}}, limit: {}) \
             {{ treePosition transactionHash }} }}",
            leaves.len()
        );
        let answer: Result<Value> = async {
            Ok(reqwest::Client::new()
                .post(&session.chain.subsquid_endpoint)
                .json(&json!({ "query": query }))
                .send()
                .await?
                .json()
                .await?)
        }
        .await;
        let Ok(answer) = answer else { continue };
        for c in answer["data"]["commitments"].as_array().into_iter().flatten() {
            let leaf = c["treePosition"].as_u64();
            let hash = c["transactionHash"].as_str();
            if let (Some(leaf), Some(hash)) = (leaf, hash) {
                session.note_tx.insert((tree, leaf as u32), hash.to_string());
            }
        }
    }
}

/// Resolves the front's asset spec (`"native"` or an ERC-20 address) and a decimal amount into an
/// `AssetId` and base units. Shielded native currency is the wrapped base token.
async fn resolve_amount(
    s: &mut Session,
    asset: &str,
    amount: &str,
) -> Result<(AssetId, u128, Address)> {
    let token = if asset.trim().eq_ignore_ascii_case("native") {
        s.chain.wrapped_base_token
    } else {
        Address::from_str(asset.trim()).map_err(|e| anyhow!("invalid token address: {e}"))?
    };
    let (_, decimals) = token_meta(s, token).await;
    let value: U256 = parse_units(amount.trim(), decimals)
        .map_err(|e| anyhow!("invalid amount: {e}"))?
        .get_absolute();
    let value = u128::try_from(value).map_err(|_| anyhow!("amount too large"))?;
    if value == 0 {
        bail!("amount must be positive");
    }
    Ok((AssetId::Erc20(token), value, token))
}

/// The public account when it lives on the Ledger's Ethereum app.
struct LedgerEoa {
    index: u32,
    /// From the Ledger key cache, read at unlock or at the first shield.
    address: Option<Address>,
}

/// IndexedDB database of the Ledger key cache, one entry per account index, shared by every
/// chain (the keys are chain-agnostic).
const LEDGER_CACHE_DB: &str = "rgw-ledger";

/// What a Ledger account exported once, so later unlocks ask the device nothing. Stored
/// unencrypted: the viewing key reads the account's history but cannot spend, and signatures
/// still come from the device.
#[derive(Serialize, Deserialize, PartialEq)]
struct LedgerKeys {
    spending_pubkey: railgun::crypto::keys::SpendingPublicKey,
    /// Hex of the viewing private key.
    viewing_key: String,
    /// The Ethereum account and its path: an address read under another derivation of the
    /// same index is not reused.
    #[serde(default)]
    eth: Option<(Address, String)>,
}

impl LedgerKeys {
    fn new(
        spending_pubkey: railgun::crypto::keys::SpendingPublicKey,
        viewing_key: &railgun::crypto::keys::ViewingKey,
        index: u32,
        eth_address: Option<Address>,
    ) -> Self {
        use railgun::crypto::keys::HexKey;
        Self {
            spending_pubkey,
            viewing_key: viewing_key.to_hex(),
            eth: eth_address.map(|a| (a, railgun_ledger::eth::eth_path(index))),
        }
    }

    fn keys(
        &self,
    ) -> Option<(railgun::crypto::keys::SpendingPublicKey, railgun::crypto::keys::ViewingKey)> {
        use railgun::crypto::keys::HexKey;
        let viewing_key = railgun::crypto::keys::ViewingKey::from_hex(&self.viewing_key).ok()?;
        Some((self.spending_pubkey, viewing_key))
    }

    fn eth_address(&self, index: u32) -> Option<Address> {
        self.eth
            .as_ref()
            .filter(|(_, path)| *path == railgun_ledger::eth::eth_path(index))
            .map(|(address, _)| *address)
    }
}

fn ledger_cache() -> Store {
    JsKvStore::store(open_db(LEDGER_CACHE_DB).unchecked_into())
}

fn ledger_keys_key(index: u32) -> Vec<u8> {
    format!("zknox:ledger-keys:{index}").into_bytes()
}

async fn read_ledger_keys(index: u32) -> Option<LedgerKeys> {
    let bytes = ledger_cache().get(ledger_keys_key(index)).await.ok().flatten()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write_ledger_keys(index: u32, keys: &LedgerKeys) {
    let Ok(bytes) = serde_json::to_vec(keys) else {
        return;
    };
    if let Err(e) = ledger_cache().put(ledger_keys_key(index), &bytes).await {
        tracing::warn!("Ledger key cache not saved: {e}");
    }
}

async fn forget_ledger_keys(index: u32) {
    let _ = ledger_cache().delete(ledger_keys_key(index)).await;
}

/// The Ethereum address of the Ledger account at `index`, read from the Ethereum app without
/// a prompt, then back to the Railgun app (best effort: a private operation reopens it).
async fn read_ledger_eth_address(index: u32) -> Result<Address> {
    let step = |msg: String| tracing::info!("{msg}");
    let device =
        switch_app(&step, railgun_ledger::eth::ETHEREUM_APP_NAME, APP_SWITCH_WAIT_MS).await?;
    let path = railgun_ledger::eth::eth_path(index);
    let read = railgun_ledger::eth::get_address(&device, &path, false).await;
    drop(device);
    let _ = switch_app(&step, railgun_ledger::eth::RAILGUN_APP_NAME, 60_000).await;
    let (_, address) = read.map_err(|e| anyhow!("Ledger Ethereum app: {e}"))?;
    let address = Address::from_str(&address).map_err(|e| anyhow!("Ledger address: {e}"))?;
    tracing::info!("Ledger public account {address} ({path})");
    Ok(address)
}

/// The public account's address, when known.
fn eoa_address(s: &Session) -> Option<Address> {
    s.eoa
        .as_ref()
        .map(EoaSigner::address)
        .or_else(|| s.eoa_ledger.as_ref().and_then(|l| l.address))
}

/// How the public account signs, resolved once per operation.
enum EoaSigning {
    /// A key on the host: the provider's wallet signs.
    Local,
    /// The Ledger's Ethereum app, reached over this connection.
    Ledger {
        device: railgun_ledger::transport::webusb::WebUsbLedger,
        path: String,
        address: Address,
    },
}

/// How long a switch between Ledger apps may take (it includes a possible confirmation on the
/// device, or a manual switch when the device does not take commands).
const APP_SWITCH_WAIT_MS: u64 = 180_000;

/// Brings `want` to the front on the Ledger and returns a connection to it.
///
/// Drives the device itself where it can: closes the running app (`quit_app`, no confirmation),
/// then launches `want` from the dashboard (`open_app`, a confirmation on some firmwares). Each
/// switch re-enumerates the device on USB, so every round opens a fresh connection. When the
/// device does not take these commands, or the user declines, it falls back to asking for a
/// manual switch and waits for it.
async fn switch_app(
    step: &dyn Fn(String),
    want: &str,
    wait_ms: u64,
) -> Result<railgun_ledger::transport::webusb::WebUsbLedger> {
    use railgun_ledger::{eth, protocol::ProtocolError, transport::webusb::WebUsbLedger};
    let deadline = now_ms() + wait_ms;
    let (mut quit_sent, mut open_sent, mut asked) = (false, false, false);
    let mut last_seen = String::new();
    loop {
        if let Ok(device) = WebUsbLedger::connect_existing().await {
            match eth::current_app(&device).await {
                Ok((name, version)) if name == want => {
                    step(format!("Ledger: {want} app {version} open"));
                    return Ok(device);
                }
                Ok((name, _)) if name == eth::DASHBOARD_NAME && !open_sent => {
                    open_sent = true;
                    step(format!(
                        "Ledger: opening the {want} app (confirm on the device if asked)"
                    ));
                    match eth::open_app(&device, want).await {
                        Ok(()) => {}
                        Err(ProtocolError::Status {
                            status: eth::STATUS_APP_NOT_INSTALLED,
                        }) => bail!("the {want} app is not installed on this Ledger"),
                        Err(e) => step(format!(
                            "Ledger did not open {want} ({e}): open it on the device"
                        )),
                    }
                }
                Ok((name, _)) if name != eth::DASHBOARD_NAME && !quit_sent => {
                    quit_sent = true;
                    step(format!("Ledger: closing the {name} app"));
                    let _ = eth::quit_app(&device).await;
                }
                Ok((name, _)) => {
                    if (open_sent || quit_sent) && !asked && name != last_seen {
                        asked = true;
                        step(format!("Ledger shows {name:?}: open the {want} app on the device"));
                    }
                    last_seen = name;
                }
                Err(_) => {}
            }
        }
        if now_ms() > deadline {
            bail!("the Ledger did not switch to the {want} app in time: nothing was signed");
        }
        gloo_timers::future::TimeoutFuture::new(1_000).await;
    }
}

async fn open_eth_app(
    shared: &SharedRef,
    id: u64,
) -> Result<railgun_ledger::transport::webusb::WebUsbLedger> {
    push_step(
        shared,
        id,
        "Ledger: switching to the Ethereum app (Blind signing must be enabled in its settings)"
            .into(),
    );
    let step = |msg| push_step(shared, id, msg);
    switch_app(&step, railgun_ledger::eth::ETHEREUM_APP_NAME, APP_SWITCH_WAIT_MS).await
}

/// After public transactions signed on the Ethereum app: back to the Railgun app, best effort
/// (the next private operation reconnects to it). Never fails the operation.
async fn back_to_railgun_app(shared: &SharedRef, id: u64, signing: &EoaSigning) {
    if !matches!(signing, EoaSigning::Ledger { .. }) {
        return;
    }
    let name = railgun_ledger::eth::RAILGUN_APP_NAME;
    let step = |msg| push_step(shared, id, msg);
    if switch_app(&step, name, 60_000).await.is_err() {
        push_step(
            shared,
            id,
            format!("reopen the {name} app on the Ledger for private operations"),
        );
    }
}

/// Resolves how this operation's public transactions get signed. For the Ledger, waits for the
/// Ethereum app, reads the account's address and caches it.
async fn eoa_signing(s: &mut Session, shared: &SharedRef, id: u64) -> Result<(EoaSigning, Address)> {
    if let Some(eoa) = &s.eoa {
        return Ok((EoaSigning::Local, eoa.address()));
    }
    let Some(ledger) = &s.eoa_ledger else {
        bail!("no public account: unlock with a public key, a phrase, or the Ledger Ethereum app");
    };
    let index = ledger.index;
    let previous = ledger.address;
    let device = open_eth_app(shared, id).await?;
    let path = railgun_ledger::eth::eth_path(index);
    let (_, address) = railgun_ledger::eth::get_address(&device, &path, false)
        .await
        .map_err(|e| anyhow!("Ledger Ethereum app: {e}"))?;
    let address = Address::from_str(&address).map_err(|e| anyhow!("Ledger address: {e}"))?;
    if let Some(prev) = previous {
        if prev != address {
            push_step(
                shared,
                id,
                format!("the Ledger account is now {address} (was {prev}): another device or passphrase?"),
            );
        }
    }
    push_step(shared, id, format!("public account {address} ({path})"));
    if let Some(mut keys) = read_ledger_keys(index).await {
        if keys.eth_address(index) != Some(address) {
            keys.eth = Some((address, path.clone()));
            write_ledger_keys(index, &keys).await;
        }
    }
    if let Some(l) = s.eoa_ledger.as_mut() {
        l.address = Some(address);
    }
    Ok((EoaSigning::Ledger { device, path, address }, address))
}

async fn send_eoa(
    s: &Session,
    shared: &SharedRef,
    id: u64,
    signing: &EoaSigning,
    label: &str,
    tx: TxData,
) -> Result<B256> {
    let pending = match signing {
        EoaSigning::Local => {
            push_step(shared, id, format!("sending {label} transaction from EOA"));
            s.provider
                .send_transaction(tx.into())
                .await
                .with_context(|| format!("{label}: eth_sendTransaction"))?
        }
        EoaSigning::Ledger { device, path, address } => {
            let raw = ledger_sign_tx(s, shared, id, device, path, *address, label, tx).await?;
            s.provider
                .send_raw_transaction(&raw)
                .await
                .with_context(|| format!("{label}: eth_sendRawTransaction"))?
        }
    };
    let hash = *pending.tx_hash();
    push_step(shared, id, format!("{label} tx {hash} sent, waiting for inclusion"));
    let receipt = pending
        .get_receipt()
        .await
        .with_context(|| format!("{label}: waiting for receipt"))?;
    if !receipt.status() {
        bail!("{label} transaction {hash} reverted");
    }
    push_step(
        shared,
        id,
        format!("{label} included in block {}", receipt.block_number.unwrap_or_default()),
    );
    Ok(hash)
}

/// Builds an EIP-1559 transaction for `tx` (nonce, fees and gas from the RPC), has the Ledger
/// Ethereum app sign it, and returns the raw signed transaction. The signer is authenticated:
/// the address recovered from the signature must be `from`.
#[allow(clippy::too_many_arguments)]
async fn ledger_sign_tx(
    s: &Session,
    shared: &SharedRef,
    id: u64,
    device: &railgun_ledger::transport::webusb::WebUsbLedger,
    path: &str,
    from: Address,
    label: &str,
    tx: TxData,
) -> Result<Vec<u8>> {
    use alloy::{
        consensus::{SignableTransaction, TxEip1559, TxEnvelope},
        eips::eip2718::Encodable2718,
        primitives::{Signature, TxKind},
    };

    let nonce = s
        .provider
        .get_transaction_count(from)
        .pending()
        .await
        .context("public account nonce")?;
    let fees = s
        .provider
        .estimate_eip1559_fees()
        .await
        .context("fee estimate")?;
    let request = alloy::rpc::types::TransactionRequest::default()
        .from(from)
        .to(tx.to)
        .input(tx.data.clone().into())
        .value(tx.value);
    let gas = s
        .provider
        .estimate_gas(request)
        .await
        .with_context(|| format!("{label}: gas estimate"))?;
    let unsigned = TxEip1559 {
        chain_id: s.chain.id,
        nonce,
        gas_limit: gas + gas / 5,
        max_fee_per_gas: fees.max_fee_per_gas,
        max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
        to: TxKind::Call(tx.to),
        value: tx.value,
        access_list: Default::default(),
        input: tx.data,
    };
    let mut payload = Vec::new();
    unsigned.encode_for_signing(&mut payload);

    push_step(
        shared,
        id,
        format!("{label}: review and approve on the Ledger (Ethereum app, blind signing)"),
    );
    let raw = railgun_ledger::eth::sign_transaction(device, path, &payload)
        .await
        .map_err(|e| anyhow!("Ledger Ethereum app ({label}): {}", railgun_ledger::eth::describe(&e)))?;

    let sighash = unsigned.signature_hash();
    let (r, sv) = (U256::from_be_bytes(raw.r), U256::from_be_bytes(raw.s));
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::new(r, sv, parity))
        .find(|sig| sig.recover_address_from_prehash(&sighash).ok() == Some(from))
        .ok_or_else(|| {
            anyhow!("the Ledger signature does not come from {from}: different account or device")
        })?;
    let envelope: TxEnvelope = unsigned.into_signed(signature).into();
    Ok(envelope.encoded_2718())
}

async fn shield(
    s: &mut Session,
    shared: &SharedRef,
    id: u64,
    asset: &str,
    amount: &str,
) -> Result<Value> {
    let (signing, eoa) = eoa_signing(s, shared, id).await?;
    // Whatever happens once on the Ethereum app, hand the device back to the Railgun app.
    let result = shield_signed(s, shared, id, &signing, eoa, asset, amount).await;
    back_to_railgun_app(shared, id, &signing).await;
    let hashes = result?;
    post_tx_sync(s, shared, id).await;
    Ok(json!({ "txHashes": hashes.iter().map(|h| h.to_string()).collect::<Vec<_>>() }))
}

#[allow(clippy::too_many_arguments)]
async fn shield_signed(
    s: &mut Session,
    shared: &SharedRef,
    id: u64,
    signing: &EoaSigning,
    eoa: Address,
    asset: &str,
    amount: &str,
) -> Result<Vec<B256>> {
    let is_native = asset.trim().eq_ignore_ascii_case("native");
    let (asset_id, value, token) = resolve_amount(s, asset, amount).await?;

    // A shield is a public transaction from this account: it pays the gas in the native
    // currency, plus the amount itself for a native shield. Say so before anything is signed.
    let native_balance = s.provider.get_balance(eoa).await.context("public account balance")?;
    let fmt = |v: U256| trim_decimal(format_units(v, 18u8).unwrap_or_default());
    if native_balance.is_zero() {
        bail!(
            "the public account {eoa} has no native currency to pay the gas: fund it, then \
             retry (nothing was signed)"
        );
    }
    if is_native && native_balance <= U256::from(value) {
        bail!(
            "the public account {eoa} holds {} native, the shield needs {} plus gas (nothing \
             was signed)",
            fmt(native_balance),
            fmt(U256::from(value))
        );
    }
    let recipient = s.signer.address();

    let mut hashes = Vec::new();
    let txs = if is_native {
        s.railgun
            .shield()
            .shield_native(recipient, value)
            .build(&mut rand::rng())?
    } else {
        let erc20 = ERC20::new(token, s.provider.clone());
        let spender = s.chain.railgun_smart_wallet;
        let balance = erc20.balanceOf(eoa).call().await?;
        if balance < U256::from(value) {
            bail!("EOA token balance {balance} is below the shield amount {value}");
        }
        let allowance = erc20.allowance(eoa, spender).call().await?;
        if allowance < U256::from(value) {
            let approve = TxData::new(
                token,
                ERC20::approveCall { spender, amount: U256::from(value) }
                    .abi_encode()
                    .into(),
                U256::ZERO,
            );
            hashes.push(send_eoa(s, shared, id, signing, "approve", approve).await?);
        }
        s.railgun
            .shield()
            .shield(recipient, asset_id, value)
            .build(&mut rand::rng())?
    };

    for tx in txs {
        hashes.push(send_eoa(s, shared, id, signing, "shield", tx).await?);
    }
    Ok(hashes)
}

#[allow(clippy::too_many_arguments)]
async fn unshield(
    s: &mut Session,
    shared: &SharedRef,
    id: u64,
    to: Address,
    asset: &str,
    amount: &str,
    native: bool,
    transport: Transport,
    fee_limits: &FeeLimits,
) -> Result<Value> {
    let (asset_id, value, token) = resolve_amount(s, asset, amount).await?;
    let signer = s.signer.clone() as Arc<dyn RailgunSigner>;

    if !native {
        let builder = TransactionBuilder::new().unshield(signer, to, asset_id, value)?;
        return submit(s, shared, id, builder, transport, Vec::new(), None, None, fee_limits).await;
    }

    if token != s.chain.wrapped_base_token {
        bail!("native unshield only applies to the wrapped base token");
    }
    if transport != Transport::Erc4337 {
        // Legacy and direct: unshield to RelayAdapt, which unwraps and forwards natively in the
        // same EVM transaction; the recipient is bound into the proofs through adaptParams.
        let relay_adapt = s.chain.relay_adapt_contract;
        let mut action = RelayAction::unshield_base_token(relay_adapt, to, &mut rand::rng());
        action.require_success = transport == Transport::Direct;
        let builder =
            action.bind(TransactionBuilder::new().unshield(signer, relay_adapt, asset_id, value)?);
        return submit(s, shared, id, builder, transport, Vec::new(), None, Some(action), fee_limits)
            .await;
    }

    // 4337: the wrapped token lands on the ephemeral 7702 sender, which unwraps it and forwards
    // the native currency during the execution phase of the same UserOperation.
    let sender = EoaSigner::random();
    let fee = value * u128::from(s.chain.unshield_fee_bps) / 10_000;
    let received = value - fee;
    let calls = vec![
        Call {
            target: token,
            value: U256::ZERO,
            data: WETH9::withdrawCall { wad: U256::from(received) }.abi_encode().into(),
        },
        Call { target: to, value: U256::from(received), data: Default::default() },
    ];
    let builder = TransactionBuilder::new().unshield(signer, sender.address(), asset_id, value)?;
    submit(s, shared, id, builder, transport, calls, Some(sender), None, fee_limits).await
}

/// Proofs (hence Ledger signatures) allowed per operation on the single-proof path: the first,
/// plus one re-proof per gas field the bundler finds too low after the fact.
const SINGLE_PROOF_ATTEMPTS: usize = 3;

/// Single-proof 4337 preparation that learns the bundler's requirements.
///
/// The fee is bound into the proof, so the gas limits must be fixed before proving. They come
/// from our own simulation, but the bundler's estimate can exceed it (Pimlico pads verification
/// by its own search ladder and multiplier: on 2026-10-07 it asked 220,825 for an account
/// verification measured at ~10k). That estimate is only available once a proof exists. On a
/// "limits too low" answer the bundler's figure is recorded and the operation re-proved with it;
/// the figures are kept for the session, so later operations fix them up front and sign once.
#[allow(clippy::too_many_arguments)]
async fn prove_once_learning(
    s: &mut Session,
    shared: &SharedRef,
    id: u64,
    builder: &TransactionBuilder,
    bundler: &PimlicoBundler,
    account: &SimpleSmartAccount,
    signer: Arc<dyn RailgunSigner>,
    fee_token: Address,
    calls: &Vec<Call>,
    has_call: bool,
    margin: u32,
) -> Result<userop_kit::signable_user_operation::SignableUserOperation, RailgunProviderError> {
    let (mut gas, _) = s
        .railgun
        .simulate_gas_limits(
            builder,
            bundler as &dyn Bundler,
            account,
            signer.clone(),
            fee_token,
            calls,
            has_call,
            margin,
            &mut rand::rng(),
        )
        .await?;

    for attempt in 1..=SINGLE_PROOF_ATTEMPTS {
        let l = s.learned_gas;
        gas.pre_verification_gas = gas.pre_verification_gas.max(l.pre_verification);
        gas.verification_gas_limit = gas.verification_gas_limit.max(l.verification);
        gas.call_gas_limit = gas.call_gas_limit.max(l.call);
        gas.paymaster_verification_gas_limit = Some(
            gas.paymaster_verification_gas_limit
                .unwrap_or(0)
                .max(l.paymaster_verification),
        );

        match s
            .railgun
            .prepare_userop_single_proof(
                builder.clone(),
                bundler as &dyn Bundler,
                account,
                signer.clone(),
                fee_token,
                calls.clone(),
                gas,
                &mut rand::rng(),
            )
            .await
        {
            Ok((signable, _report)) => return Ok(signable),
            Err(RailgunProviderError::LimitsTooLow { what, needed, limit })
                if attempt < SINGLE_PROOF_ATTEMPTS =>
            {
                let learned = &mut s.learned_gas;
                let slot = match what {
                    "pre-verification" => &mut learned.pre_verification,
                    "account verification" => &mut learned.verification,
                    "call" => &mut learned.call,
                    "paymaster verification" => &mut learned.paymaster_verification,
                    _ => return Err(RailgunProviderError::LimitsTooLow { what, needed, limit }),
                };
                *slot = (*slot).max(needed);
                push_step(
                    shared,
                    id,
                    format!(
                        "the bundler wants {needed} gas for {what} (limit was {limit}): nothing \
                         was sent; proving again with it (one more signature), remembered for \
                         the next operations of this session"
                    ),
                );
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the last attempt returns")
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

/// Proves and submits a private transaction over the selected transport. `relay` is the
/// RelayAdapt action `builder` was bound to, if any: the proved transaction is then sent as
/// `RelayAdapt.relay(...)`.
#[allow(clippy::too_many_arguments)]
async fn submit(
    s: &mut Session,
    shared: &SharedRef,
    id: u64,
    builder: TransactionBuilder,
    transport: Transport,
    calls: Vec<Call>,
    sender: Option<EoaSigner>,
    relay: Option<RelayAction>,
    fee_limits: &FeeLimits,
) -> Result<Value> {
    if relay.is_some() && transport == Transport::Erc4337 {
        bail!("RelayAdapt actions are for the legacy and direct transports");
    }
    if s.ledger_usb {
        // The spending signature comes from the Railgun app: bring it to the front if another
        // app is open (the Ethereum app after a shield). The signer reconnects on its own.
        let step = |msg| push_step(shared, id, msg);
        switch_app(&step, railgun_ledger::eth::RAILGUN_APP_NAME, APP_SWITCH_WAIT_MS).await?;
    }
    let result = match transport {
        Transport::Legacy => {
            if !calls.is_empty() {
                bail!(
                    "the legacy transport runs post-transaction calls through RelayAdapt \
                     (RelayAction), not through a sender account"
                );
            }
            submit_legacy(s, shared, id, builder, relay.as_ref(), fee_limits).await?
        }
        Transport::Direct => {
            if s.eoa.is_none() && s.eoa_ledger.is_none() {
                bail!("direct transport needs a public account");
            }
            // Prove first: the spending signature comes from the Railgun app; only then is the
            // Ethereum app needed, to send.
            push_step(shared, id, "building and proving (direct transport)".into());
            let mut proved = s.railgun.build(builder, &mut rand::rng()).await?;
            if let Some(action) = &relay {
                action.package(&mut proved);
            }
            push_step(shared, id, format!("proved {} operation(s)", proved.proved_operations.len()));
            let (signing, _) = eoa_signing(s, shared, id).await?;
            let sent = send_eoa(s, shared, id, &signing, "transact", proved.tx_data).await;
            back_to_railgun_app(shared, id, &signing).await;
            let hash = sent?;
            json!({ "transport": "direct", "txHash": hash.to_string() })
        }
        Transport::Erc4337 => {
            let sender = sender.unwrap_or_else(EoaSigner::random);
            let funds_transit = !calls.is_empty();
            if funds_transit {
                // No filesystem in the browser: surface the key in the job steps so it can be
                // saved. If the execution phase reverts after the paymaster unshielded, the funds
                // sit on this address.
                push_step(
                    shared,
                    id,
                    format!(
                        "ephemeral sender {} key 0x{} — keep it until the operation completes",
                        sender.address(),
                        hex::encode(sender.to_bytes())
                    ),
                );
            }
            let bundler = PimlicoBundler::new(
                s.bundler_url
                    .parse()
                    .map_err(|e| anyhow!("invalid bundler url: {e}"))?,
            );
            let account = SimpleSmartAccount::new(sender.address(), s.chain.id, Arc::new(s.provider.clone()));
            let railgun_signer = s.signer.clone() as Arc<dyn RailgunSigner>;
            let fee_token = s.chain.wrapped_base_token;
            let has_call = !calls.is_empty();
            let margin = fee_limits
                .gas_margin_percent
                .unwrap_or(DEFAULT_GAS_MARGIN_PERCENT)
                .min(MAX_GAS_MARGIN_PERCENT);

            let (mut signable, mode) = if fee_limits.single_proof {
                push_step(
                    shared,
                    id,
                    format!(
                        "proving once from ephemeral sender {} (gas simulated with {margin}% margin)",
                        sender.address()
                    ),
                );
                match prove_once_learning(
                    s,
                    shared,
                    id,
                    &builder,
                    &bundler,
                    &account,
                    railgun_signer.clone(),
                    fee_token,
                    &calls,
                    has_call,
                    margin,
                )
                .await
                {
                    Ok(signable) => (signable, "single-proof"),
                    Err(e) if fee_limits.single_proof_strict => {
                        return Err(anyhow!(e).context(
                            "the single-proof path failed and falling back to several \
                             signatures is disabled. Nothing was sent",
                        ));
                    }
                    Err(e) => {
                        push_step(
                            shared,
                            id,
                            format!(
                                "gas limits could not be simulated ({e:#}): falling back to the \
                                 iterative estimate, which signs several times"
                            ),
                        );
                        let signable = s
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
                push_step(
                    shared,
                    id,
                    format!(
                        "preparing UserOperation from ephemeral sender {} (proof is regenerated \
                         until the fee converges)",
                        sender.address()
                    ),
                );
                let signable = s
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
            push_step(
                shared,
                id,
                format!(
                    "fee {} wrapped token ({mode})",
                    trim_decimal(format_units(U256::from(max_fee), 18u8).unwrap_or_default())
                ),
            );
            if let Some(warning) = refresh_priority_fee(&bundler, &mut signable).await {
                push_step(shared, id, warning);
            }
            let signed = signable.sign(&sender).await?;
            let hash = bundler.send_user_operation(&signed).await?;
            push_step(shared, id, format!("UserOperation {:?} sent, waiting for receipt", hash.0));
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
    post_tx_sync(s, shared, id).await;
    Ok(result)
}

/// Live view of the broadcaster network for the front's legacy panel, one refresh every 3s.
/// Stops when `alive` is cleared (lock or re-unlock).
async fn fee_monitor(
    client: Arc<BroadcasterClient>,
    shared: SharedRef,
    mode: &'static str,
    waku_url: String,
    alive: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut subscribed = false;
    while alive.load(std::sync::atomic::Ordering::Relaxed) {
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
        if !alive.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if let Ok(mut l) = shared.legacy.write() {
            *l = status;
        }
        gloo_timers::future::TimeoutFuture::new(3_000).await;
    }
}

/// Rate as a multiple of the gas cost, two decimals: 1.12e18 -> "1.12".
fn format_rate(rate: u128) -> String {
    let hundredths = rate / (PAR_RATE_WRAPPED_BASE_TOKEN / 100);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// `eth_estimateGas` on a zero-proof build, from the verification-bypass origin (the Railgun
/// verifier skips the SNARK check for it). With a gas price the node caps gas by the sender's
/// balance, which may be empty: retried with a balance override.
async fn estimate_with_dummy_proof(
    s: &mut Session,
    builder: TransactionBuilder,
    gas_price: Option<u128>,
    relay: Option<&RelayAction>,
) -> Result<u64> {
    let mut dummy = s.railgun.build_dummy(builder, &mut rand::rng()).await?;
    if let Some(action) = relay {
        action.package(&mut dummy);
    }
    let mut request: alloy::rpc::types::TransactionRequest = dummy.tx_data.into();
    request = request.from(railgun::provider::VERIFICATION_BYPASS);
    if let Some(gas_price) = gas_price {
        request = request.gas_price(gas_price);
    }
    match s.provider.estimate_gas(request.clone()).await {
        Ok(gas) => Ok(gas),
        Err(e) if e.to_string().to_lowercase().contains("insufficient funds") => {
            use alloy::rpc::types::state::{AccountOverride, StateOverride};
            let mut overrides = StateOverride::default();
            overrides.insert(
                railgun::provider::VERIFICATION_BYPASS,
                AccountOverride::default().with_balance(U256::from(10u128.pow(24))),
            );
            Ok(s.provider
                .estimate_gas(request)
                .overrides(overrides)
                .await
                .context("gas estimate with balance override")?)
        }
        Err(e) => Err(e.into()),
    }
}

/// Relays through a Railgun community broadcaster: quote, fee from a dummy-proof gas estimate,
/// proof with the fee note pinned first and `minGasPrice` bound, pre-transaction POIs, sealed
/// request over Waku. Port of the daemon's `submit_legacy`.
async fn submit_legacy(
    s: &mut Session,
    shared: &SharedRef,
    id: u64,
    builder: TransactionBuilder,
    relay: Option<&RelayAction>,
    fee_limits: &FeeLimits,
) -> Result<Value> {
    // Real funds: never pay an uncapped rate. The reference client makes the trusted signer
    // mandatory everywhere; test networks stay usable without one.
    if s.chain.id == 1 && s.broadcaster.trusted_signer_count() == 0 {
        bail!(
            "on mainnet the legacy transport needs a trusted fee signer: reopen the wallet \
             with the 0zk address of one, so broadcaster rates are capped"
        );
    }
    let fee_token = s.chain.wrapped_base_token;
    let fee_asset = AssetId::Erc20(fee_token);
    let list_keys = s.railgun.poi_list_keys();

    let rate_multiple = fee_limits
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
        .ok_or_else(|| {
            anyhow!("invalid maximum fee rate \"{rate_multiple}\" (expected a multiple of the gas cost such as 1.5)")
        })?;
    let max_fee: Option<u128> = match fee_limits.max_fee.as_deref().map(str::trim) {
        Some(v) if !v.is_empty() => Some(
            parse_units(v, 18u8)
                .ok()
                .and_then(|v| u128::try_from(v.get_absolute()).ok())
                .ok_or_else(|| anyhow!("invalid maximum fee \"{v}\""))?,
        ),
        _ => None,
    };

    s.silent_broadcasters
        .retain(|(_, since)| since.elapsed().as_millis() < u128::from(SILENT_BROADCASTER_PENALTY_MS));
    let chosen = fee_limits
        .broadcaster
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string);
    let selection = match &chosen {
        // An explicit choice is honoured even if that broadcaster was silent before.
        Some(address) => s
            .broadcaster
            .quotes_for(&fee_token.to_string(), &list_keys)
            .into_iter()
            .find(|q| &q.railgun_address == address && q.fee_per_unit_gas <= max_rate)
            .ok_or(NoQuote::None),
        None => {
            let exclude: Vec<String> =
                s.silent_broadcasters.iter().map(|(a, _)| a.clone()).collect();
            s.broadcaster.select_quote(
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
    push_step(
        shared,
        id,
        format!(
            "broadcaster {}… ({}), rate {} times the gas cost (ceiling {rate_multiple}), quote valid {}s",
            &quote.railgun_address[..quote.railgun_address.len().min(14)],
            quote.identifier.as_deref().unwrap_or("no identifier"),
            format_rate(quote.fee_per_unit_gas),
            quote.expiration.saturating_sub(quote_now_ms()) / 1000,
        ),
    );

    // Broadcasters send type 1 transactions at exactly minGasPrice and refuse a price under half
    // the slow market price: take the node's price plus 10%.
    let gas_price = s.provider.get_gas_price().await.context("eth_gasPrice")? * 11 / 10;

    let signer = s.signer.clone() as Arc<dyn RailgunSigner>;
    let with_fee = |fee: u128| -> Result<TransactionBuilder> {
        Ok(builder
            .clone()
            .broadcaster_fee(signer.clone(), broadcaster_address.clone(), fee_asset, fee)?
            .min_gas_price(gas_price))
    };
    let fee_for = |gas: u64| {
        token_fee(quote.fee_per_unit_gas, gas, gas_price).ok_or_else(|| anyhow!("fee overflow"))
    };

    // The fee changes note values, possibly the number of inputs, hence the gas: estimate twice,
    // the second time with a fee of the right magnitude.
    let mut fee = fee_for(FIRST_GUESS_GAS)?;
    let mut gas = 0;
    for round in 1..=2 {
        gas = estimate_with_dummy_proof(s, with_fee(fee)?, Some(gas_price), relay)
            .await
            .with_context(|| format!("dummy-proof gas estimate (round {round})"))?;
        fee = fee_for(gas)?;
    }
    push_step(
        shared,
        id,
        format!(
            "gas estimate {gas}, gas price {gas_price} wei, broadcaster fee {} wrapped token",
            trim_decimal(format_units(U256::from(fee), 18u8).unwrap_or_default())
        ),
    );

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

    push_step(shared, id, "building and proving (legacy transport)".into());
    let mut proved = s.railgun.build(with_fee(fee)?, &mut rand::rng()).await?;
    if let Some(action) = relay {
        action.package(&mut proved);
    }
    push_step(
        shared,
        id,
        format!("proved {} operation(s), generating pre-transaction POI", proved.proved_operations.len()),
    );
    let pre_transaction_pois =
        railgun_broadcaster::pre_transaction_pois(&s.railgun, &proved.proved_operations).await?;

    if !quote.usable_at(quote_now_ms().saturating_sub(30_000)) {
        bail!("the fee quote expired while proving, run the operation again");
    }

    let use_relay_adapt = relay.is_some();
    if use_relay_adapt {
        push_step(
            shared,
            id,
            "submitting through RelayAdapt.relay (unwrap and recipient bound in adaptParams)".into(),
        );
    }
    let sealed = s.broadcaster.seal(
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

    let stats_before = s.broadcaster.publish_stats();
    push_step(shared, id, "request sealed and published, waiting for the broadcaster (up to 120s)".into());
    let outcome = s.broadcaster.send(&sealed).await;
    match outcome {
        Ok(tx_hash) => {
            push_step(shared, id, format!("broadcaster sent transaction {tx_hash}"));
            Ok(json!({
                "transport": "legacy",
                "txHash": tx_hash,
                "broadcaster": quote.railgun_address,
                "fee": fee.to_string(),
                "minGasPrice": gas_price.to_string(),
            }))
        }
        Err(ClientError::Timeout(_)) => {
            // Did the request leave the page at all? A push that fails looks the same as a
            // broadcaster that stays silent, and the remedy is not.
            let delivery = match (s.broadcaster.publish_stats(), stats_before) {
                (Some(after), Some(before)) => {
                    let delivered = after.delivered - before.delivered;
                    let failed = after.failed - before.failed;
                    let unacked = (after.queued - before.queued).saturating_sub(delivered + failed);
                    let via = if s.bridge.is_some() { "through the tab" } else { "by the worker's node" };
                    push_step(
                        shared,
                        id,
                        format!(
                            "publishes {via}: {delivered} accepted by a Waku peer, {failed} failed{}{}",
                            after.last_error.as_ref().filter(|_| failed > 0).map(|e| format!(" ({e})")).unwrap_or_default(),
                            if unacked > 0 { format!(", {unacked} never acknowledged") } else { String::new() }
                        ),
                    );
                    Some(delivered)
                }
                _ => None,
            };
            if delivery != Some(0) {
                s.silent_broadcasters
                    .push((quote.railgun_address.clone(), web_time::Instant::now()));
            }
            // No answer is not an outcome. The chain is: a spent input means the transaction was
            // mined, whoever sent it.
            push_step(
                shared,
                id,
                "no answer from the broadcaster within 120s: checking on-chain whether the inputs were spent".into(),
            );
            let inputs: Vec<(u32, u32)> = proved
                .proved_operations
                .iter()
                .flat_map(|op| op.inner.in_notes().iter().map(|n| (n.tree_number, n.leaf_index)))
                .collect();
            let address = s.signer.address();
            for attempt in 1..=4 {
                gloo_timers::future::TimeoutFuture::new(20_000).await;
                if let Err(e) = s.railgun.sync().await {
                    push_step(shared, id, format!("sync {attempt}/4 failed: {e}"));
                    continue;
                }
                let unspent = s.railgun.notes(address.clone()).await;
                let still_there = inputs
                    .iter()
                    .filter(|(t, l)| unspent.iter().any(|n| n.tree_number == *t && n.leaf_index == *l))
                    .count();
                if still_there == 0 {
                    push_step(
                        shared,
                        id,
                        "the input notes are spent: the transaction was mined, the broadcaster's answer was lost".into(),
                    );
                    return Ok(json!({
                        "transport": "legacy",
                        "txHash": null,
                        "outcome": "mined, answer lost",
                        "broadcaster": quote.railgun_address,
                        "fee": fee.to_string(),
                    }));
                }
                push_step(
                    shared,
                    id,
                    format!("check {attempt}/4: {still_there} of {} input note(s) still unspent", inputs.len()),
                );
            }
            let dropped = s
                .railgun
                .discard_pending_poi(&proved.proved_operations)
                .await
                .unwrap_or(0);
            bail!(
                "the broadcaster never answered and, 80s later, the input notes are still \
                 unspent: the transaction was not sent. Nothing was paid. {dropped} pending \
                 POI entr{} dropped. {}",
                if dropped == 1 { "y" } else { "ies" },
                if delivery == Some(0) && s.bridge.is_some() {
                    "The request never left this tab: reload the page so its Waku node \
                     reconnects, then retry."
                } else if delivery == Some(0) {
                    "No Waku peer accepted the request: check the state of the Waku node in \
                     the legacy panel and the log, then retry."
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

async fn post_tx_sync(s: &mut Session, shared: &SharedRef, id: u64) {
    push_step(
        shared,
        id,
        format!("waiting {}s for indexing, then syncing", POST_TX_SYNC_DELAY_MS / 1000),
    );
    gloo_timers::future::TimeoutFuture::new(POST_TX_SYNC_DELAY_MS).await;
    if let Err(e) = s.railgun.sync().await {
        push_step(shared, id, format!("post-transaction sync failed (run a manual sync): {e}"));
    }
}

/// The same-origin POI relay URL, when this host serves one. Probed with a JSON-RPC POST: the
/// real POI node answers JSON (a `jsonrpc` error for a bad method is fine); a host without the
/// relay answers its 404 page, which does not parse.
async fn poi_relay() -> Option<String> {
    let scope: web_sys::WorkerGlobalScope = js_sys::global().dyn_into().ok()?;
    let origin = scope.location().origin();
    if origin.is_empty() || !origin.starts_with("http") {
        return None;
    }
    let relay = format!("{origin}/poi");
    let body: Value = reqwest::Client::new()
        .post(&relay)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ppoi_node_status", "params": [] }))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    body.get("jsonrpc").is_some().then_some(relay)
}

fn default_rpc_url(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("https://ethereum-rpc.publicnode.com"),
        11155111 => Some("https://ethereum-sepolia-rpc.publicnode.com"),
        _ => None,
    }
}

fn default_bundler_url(chain_id: u64) -> String {
    format!("https://public.pimlico.io/v2/{chain_id}/rpc")
}

fn trim_decimal(s: String) -> String {
    if !s.contains('.') {
        return s;
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

// ---------------------------------------------------------------------------- routes

/// Non-secret defaults the front prefills the unlock form with — mirrors `ipc::defaults`. The
/// "native" Waku node here is the Rust light node running in the engine worker (waku-light over
/// the browser's WebSocket); "browser" (js-waku in the tab, bridged over `/api/waku/exchange`)
/// and "nwaku" work as on the daemon.
fn defaults() -> Value {
    json!({
        "version": VERSION,
        "nativeWaku": true,
        "waku": {
            "clusterId": railgun_broadcaster::wire::CLUSTER_ID,
            "shardId": railgun_broadcaster::wire::SHARD_ID,
            "bootstrapPeers": railgun_broadcaster::wire::FLEET_WSS_PEERS,
        },
        "trustedFeeSigners": railgun_broadcaster::RAILWAY_TRUSTED_FEE_SIGNERS,
        "trustedFeeSignersSource": "Railway wallet remote configuration",
    })
}

/// Round trip with the Waku node of the front's tab ("browser" mode): takes what it received,
/// returns what it must publish. Port of the daemon's `ipc::waku_exchange`.
fn waku_exchange(sh: &SharedRef, body: &str) -> Result<Value, String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use railgun_broadcaster::{PublishAck, RemoteStatus, transport::WakuMessage};

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ExchangeMessage {
        content_topic: String,
        /// base64
        payload: String,
        #[serde(default)]
        timestamp_ns: Option<String>,
    }

    #[derive(Deserialize)]
    struct ExchangeAck {
        id: u64,
        #[serde(default)]
        peers: usize,
        #[serde(default)]
        error: Option<String>,
    }

    #[derive(Deserialize)]
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
    let publish: Vec<Value> = sh
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
                .map(|a| PublishAck { id: a.id, peers: a.peers, error: a.error })
                .collect(),
        )
        .into_iter()
        .map(|o| json!({ "id": o.id, "contentTopic": o.content_topic, "payload": STANDARD.encode(&o.payload) }))
        .collect();
    Ok(json!({ "publish": publish }))
}

fn status_value(sh: &SharedRef) -> Value {
    let snapshot = sh.status.read().map(|s| s.clone()).unwrap_or_default();
    let active = sh.jobs.lock().ok().and_then(|j| j.active());
    let legacy = sh.legacy.read().map(|l| l.clone()).unwrap_or_default();
    json!({ "status": snapshot, "activeJob": active, "legacy": legacy })
}

fn since_of(path: &str, body: &str) -> u64 {
    if let Some(query) = path.split('?').nth(1) {
        for kv in query.split('&') {
            if let Some(v) = kv.strip_prefix("since=") {
                return v.parse().unwrap_or(0);
            }
        }
    }
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("since").and_then(Value::as_u64))
        .unwrap_or(0)
}

fn parse<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, String> {
    serde_json::from_str(if body.trim().is_empty() { "{}" } else { body })
        .map_err(|e| format!("bad request body: {e}"))
}

async fn route(method: &str, path: &str, body: &str) -> Result<Value, String> {
    let sh = shared_ref();
    let path_only = path.split('?').next().unwrap_or(path);
    match path_only {
        "/api/defaults" => Ok(defaults()),
        "/api/status" => Ok(status_value(&sh)),
        "/api/jobs" => Ok(json!({ "jobs": sh.jobs.lock().map(|j| j.all()).unwrap_or_default() })),
        "/api/logs" => Ok(json!({
            "lines": sh.logs.lock().map(|l| l.since(since_of(path, body))).unwrap_or_default()
        })),
        "/api/unlock" => {
            let p: UnlockParams = parse(body)?;
            let (tx, rx) = oneshot::channel();
            send(Command::Unlock(Box::new(p), tx)).map_err(|e| format!("{e:#}"))?;
            match rx.await {
                Ok(Ok(())) => Ok(json!({ "ok": true })),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("engine dropped the request".into()),
            }
        }
        "/api/op" => {
            let op: Op = parse(body)?;
            if !sh.status.read().map(|s| s.unlocked).unwrap_or(false) {
                return Err("wallet is locked".into());
            }
            let id = sh
                .jobs
                .lock()
                .map(|mut j| j.create(op.kind()))
                .map_err(|_| "state poisoned".to_string())?;
            send(Command::Job { id, op }).map_err(|e| format!("{e:#}"))?;
            Ok(json!({ "jobId": id }))
        }
        "/api/lock" => {
            let (tx, rx) = oneshot::channel();
            send(Command::Lock(tx)).map_err(|e| format!("{e:#}"))?;
            rx.await.map_err(|_| "engine dropped the request".to_string())?;
            Ok(json!({ "ok": true }))
        }
        "/api/empty-cache" => {
            let (tx, rx) = oneshot::channel();
            send(Command::EmptyCache(tx)).map_err(|e| format!("{e:#}"))?;
            match rx.await {
                Ok(Ok(removed)) => Ok(json!({ "ok": true, "removed": removed })),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("engine dropped the request".into()),
            }
        }
        "/api/waku/exchange" => waku_exchange(&sh, body),
        other => match other.strip_prefix("/api/jobs/") {
            Some(id) => id
                .parse::<u64>()
                .ok()
                .and_then(|id| sh.jobs.lock().ok().and_then(|j| j.get(id)))
                .map(|job| serde_json::to_value(job).unwrap_or(Value::Null))
                .ok_or_else(|| "unknown job".to_string()),
            None => Err(format!("no route {method} {path}")),
        },
    }
}

/// Entry point used by `web/worker.js`: one call per `/api/…` request. On an API error the returned
/// promise rejects, so the front's `fetch` wrapper throws exactly as it would against the daemon.
#[wasm_bindgen]
pub async fn api(method: String, path: String, body: String) -> Result<String, JsValue> {
    route(&method.to_ascii_uppercase(), &path, &body)
        .await
        .map(|v| v.to_string())
        .map_err(|e| JsValue::from_str(&e))
}

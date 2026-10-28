//! Session core shared by the native daemon (`engine.rs`) and the web build
//! (`railgun-viewer-web`): unlock parameters, signer resolution, the per-account session and the
//! history snapshot. No thread, no file, no clock of its own: persistence goes through
//! [`Store`], progress and log lines through [`SharedRef`].

use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
};

use alloy::{
    primitives::{Address, U256},
    providers::{DynProvider, Provider},
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use railgun::{
    RailgunSnapshot, RailgunState,
    account::{
        address::RailgunAddress,
        signer::{PrivateKeySigner as RgSigner, RailgunSigner},
    },
    caip::AssetId,
    chain_config::ChainConfig,
    provider::{RailgunProvider, SentNote, UtxoNote, blinded_commitment},
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    chain::{self, ChainRef, OpRef, Squid, TokenMeta, UnshieldRef},
    history::{self, NoteIn, OpIn, SentIn},
    keys::{self, Credentials, Resolved},
    shared::{SharedRef, log, now_ms},
    signer::{MasterSigner, SharedSigner},
    wallet_keys::Derivation,
};

pub fn default_rpc_url(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("https://ethereum-rpc.publicnode.com"),
        11155111 => Some("https://ethereum-sepolia-rpc.publicnode.com"),
        56 => Some("https://bsc-rpc.publicnode.com"),
        137 => Some("https://polygon-bor-rpc.publicnode.com"),
        42161 => Some("https://arbitrum-one-rpc.publicnode.com"),
        _ => None,
    }
}

pub fn explorer(chain_id: u64) -> String {
    match chain_id {
        1 => "https://etherscan.io/tx/",
        11155111 => "https://sepolia.etherscan.io/tx/",
        137 => "https://polygonscan.com/tx/",
        42161 => "https://arbiscan.io/tx/",
        56 => "https://bscscan.com/tx/",
        _ => "",
    }
    .to_string()
}

/// Unlock request. Flat on purpose (no `serde(flatten)`): the credential fields are read here and
/// copied into [`Credentials`].
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UnlockParams {
    #[serde(default)]
    pub mnemonic: Option<String>,
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub derivation: Derivation,
    #[serde(default, alias = "viewing_key", alias = "viewingPrivateKey", alias = "key")]
    pub viewing_key: Option<String>,
    /// 0zk address, optional with a bare viewing key: it carries the master public key, which
    /// spares the on-chain discovery scan. Ignored with a mnemonic.
    #[serde(default, alias = "zkAddress", alias = "railgunAddress")]
    pub address: Option<String>,
    /// Legacy format (Railway / community engine shareable viewing key, msgpack `{vpriv, spub}`).
    #[serde(default, alias = "legacyKey", alias = "shareableViewingKey", alias = "shareable_key")]
    pub shareable_key: Option<String>,
    pub chain_id: u64,
    pub rpc_url: Option<String>,
    #[serde(default = "default_true")]
    pub poi: bool,
    #[serde(default = "default_true")]
    pub sync_now: bool,
}

impl UnlockParams {
    pub fn credentials(&self) -> Credentials {
        Credentials {
            mnemonic: self.mnemonic.clone(),
            index: self.index,
            derivation: self.derivation,
            viewing_key: self.viewing_key.clone(),
            shareable_key: self.shareable_key.clone(),
        }
    }

    /// Checks done before replying to the unlock request: key syntax, and the 0zk address
    /// against the viewing key when both are given.
    pub fn precheck(&self) -> Result<()> {
        let r = keys::resolve(&self.credentials())?;
        if let Resolved::ViewOnly { viewing } = &r {
            match keys::non_empty(&self.address) {
                Some(a) => {
                    keys::master_from_address(a, viewing)?;
                }
                None => bail!(NO_ADDRESS),
            }
        }
        Ok(())
    }

    /// Lengths of what was received, for the log when the request is refused.
    pub fn lengths(&self) -> String {
        format!(
            "unlock: received mnemonic={} viewingKey={} address={} legacyKey={} (lengths)",
            self.mnemonic.as_deref().map(str::len).unwrap_or(0),
            self.viewing_key.as_deref().map(str::len).unwrap_or(0),
            self.address.as_deref().map(str::len).unwrap_or(0),
            self.shareable_key.as_deref().map(str::len).unwrap_or(0)
        )
    }

    pub fn rpc_url(&self, chain: &ChainConfig) -> Result<String> {
        let url = match self.rpc_url.as_deref().map(str::trim) {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => default_rpc_url(chain.id)
                .ok_or_else(|| anyhow!("no default RPC for chain {}", chain.id))?
                .to_string(),
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("RPC URL must start with http(s)://, got \"{url}\"");
        }
        Ok(url)
    }
}

fn default_true() -> bool {
    true
}

/// A bare viewing key is refused: the on-chain discovery of the master public key (first transfer
/// received in clear) was slow and unreliable, see ADR-016.
const NO_ADDRESS: &str = "a private viewing key alone is not accepted: give the 0zk address with it, \
or use the legacy format (Railway shareable viewing key)";

/// Signer for the unlock request: full keys from a mnemonic, else a view-only signer whose master
/// public key is read from the 0zk address, or computed from the legacy format.
pub async fn make_signer(
    resolved: Resolved,
    address: Option<&str>,
    chain: &ChainConfig,
    shared: &SharedRef,
) -> Result<(Arc<dyn RailgunSigner>, &'static str)> {
    match resolved {
        Resolved::Full { spending, viewing, scheme } => Ok((RgSigner::new_evm(spending, viewing, chain.id), scheme)),
        Resolved::Shared { viewing, spub } => {
            let signer = SharedSigner::new(viewing, spub, chain.id);
            if let Some(a) = address {
                let given = keys::parse_address(a)?;
                if given.to_string() != signer.address().to_string() {
                    bail!("this legacy key does not belong to that 0zk address");
                }
            }
            log(shared, "✓ legacy key: master public key computed from the spending public key and the viewing key (no scan)");
            Ok((signer, "n/a"))
        }
        Resolved::ViewOnly { viewing } if address.is_some() => {
            let master = keys::master_from_address(address.unwrap_or_default(), &viewing)?;
            log(shared, "✓ master public key read from the 0zk address (viewing key checked against it)");
            Ok((MasterSigner::new(viewing, master, chain.id), "n/a"))
        }
        Resolved::ViewOnly { .. } => bail!(NO_ADDRESS),
    }
}

fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {:02} s", s / 60, s % 60),
        _ => format!("{} h {:02} min", s / 3600, (s % 3600) / 60),
    }
}

/// Sync with progress: the range from the last synced block to the head is synced in steps
/// (about 25, at least 20 000 blocks each), the SDK state saved after each step so that an
/// interrupted sync resumes where it stopped, with percentage, block and estimated time left in
/// the status line. A short range is a single step. The caller saves the state once more at the
/// end and reports a save failure; here a failed save is only logged, the next one retries.
pub async fn sync_with_progress(session: &mut Session, shared: &SharedRef) -> Result<()> {
    let head = session.provider.get_block_number().await.map_err(|e| anyhow!("eth_blockNumber: {e}"))?;
    let start = session.railgun.synced_block().max(session.chain.deployment_block.saturating_sub(1));
    if head <= start {
        return session.railgun.sync_without_poi_submission().await.map_err(|e| anyhow!("{e:#}"));
    }
    let total = head - start;
    let step = (total / 25).max(20_000);
    let t0 = now_ms();
    let progress = |shared: &SharedRef, synced: u64| {
        let done = synced.saturating_sub(start).min(total);
        let pct = done * 100 / total;
        let elapsed = now_ms().saturating_sub(t0);
        let eta = if done > 0 && done < total {
            format!(" · ~{} left", fmt_duration(elapsed.saturating_mul(total - done) / done))
        } else {
            String::new()
        };
        if let Ok(mut s) = shared.lock() {
            s.stage = Some("syncing");
            s.stage_detail = Some(format!("{pct} % · block {} / {head}{eta}", synced.max(start)));
            s.updated_at = now_ms();
        }
    };
    progress(shared, start);
    let mut target = start;
    while target < head {
        target = (target + step).min(head);
        session.railgun.sync_to_without_poi_submission(target).await.map_err(|e| anyhow!("{e:#}"))?;
        if let Err(e) = session.persist_state().await {
            tracing::warn!("Railgun state not saved after this step: {e:#}");
        }
        progress(shared, session.railgun.synced_block());
    }
    // blocks mined while syncing
    session.railgun.sync_without_poi_submission().await.map_err(|e| anyhow!("{e:#}"))?;
    log(
        shared,
        format!("sync: {total} block(s) in {}", fmt_duration(now_ms().saturating_sub(t0))),
    );
    Ok(())
}

/// After unlock: says plainly when this network cannot reach the POI node (statuses would stay
/// unknown without explanation). Same check and wording in the daemon and in the web build.
pub async fn warn_if_poi_unreachable(chain_id: u64, endpoint: &str, shared: &SharedRef) {
    if let Err(e) = crate::health::poi_reachable(chain_id, endpoint).await {
        let hint = if endpoint.contains("ppoi.fdi.network") {
            format!(" {}", crate::health::POI_IPV6_HINT)
        } else {
            String::new()
        };
        let msg = format!(
            "POI node unreachable from this network ({endpoint}): POI statuses will stay unknown.{hint} ({e})"
        );
        log(shared, format!("✗ {msg}"));
        if let Ok(mut s) = shared.lock() {
            s.last_error = Some(msg);
            s.updated_at = now_ms();
        }
    }
}

/// Where the account's data is persisted: files in the account's directory on the daemon,
/// IndexedDB in the web build. Two documents: the SDK state (trees, sync cursors, decrypted
/// notes, POI statuses), which the SDK keeps in memory and the viewer saves as a snapshot after
/// every operation that can change it, and the viewer cache (chain references, POI statuses,
/// operations), which is only a cache.
///
/// Both builds are single-threaded for the session (one engine thread, one worker), so the
/// futures need not be `Send`.
#[async_trait(?Send)]
pub trait Store {
    /// Saves the viewer cache. Best effort: the next refresh rewrites it.
    fn save_cache(&self, value: Value);
    /// Saves the serialized SDK state, replacing the previous one atomically: a crash during the
    /// save must leave the previous snapshot in place rather than a truncated one.
    async fn save_state(&self, bytes: Vec<u8>) -> Result<()>;
}

/// Key of the SDK state in the web build's IndexedDB, and stem of the daemon's file name.
#[allow(dead_code, reason = "the IndexedDB key of the web build; the native build names a file")]
pub const STATE_KEY: &str = "railgun-state";

/// The SDK state as saved: a postcard-encoded [`RailgunSnapshot`]. Postcard is compact and
/// keeps the snapshot's binary keys and values as bytes, where JSON would expand them to arrays
/// of numbers.
pub fn state_bytes(state: &RailgunState) -> Result<Vec<u8>> {
    let snapshot = state.snapshot().context("exporting the Railgun state")?;
    postcard::to_allocvec(&snapshot).context("encoding the Railgun state")
}

/// The state saved by [`state_bytes`], restored as an independent in-memory state.
pub fn state_from_bytes(bytes: &[u8]) -> Result<RailgunState> {
    let snapshot: RailgunSnapshot = postcard::from_bytes(bytes).context("decoding the Railgun state")?;
    RailgunState::from_snapshot(snapshot).context("restoring the Railgun state")
}

pub type PoiCache = HashMap<String, std::collections::BTreeMap<String, String>>;

/// Viewer cache, kept outside the SDK state.
#[derive(Default)]
pub struct Caches {
    pub commitment_refs: HashMap<String, ChainRef>,
    pub nullifier_refs: HashMap<String, ChainRef>,
    pub poi_cache: PoiCache,
    pub op_refs: HashMap<String, OpRef>,
    pub chain_ops: HashMap<String, OpIn>,
}

impl Caches {
    pub fn from_value(v: &Value) -> Self {
        fn take<T: serde::de::DeserializeOwned + Default>(v: &Value, k: &str) -> T {
            v.get(k).cloned().and_then(|x| serde_json::from_value(x).ok()).unwrap_or_default()
        }
        Caches {
            commitment_refs: take(v, "commitments"),
            nullifier_refs: take(v, "nullifiers"),
            poi_cache: take(v, "poi"),
            op_refs: take(v, "ops"),
            chain_ops: take(v, "chainOps"),
        }
    }
}

pub struct Session {
    pub chain: ChainConfig,
    pub provider: DynProvider,
    pub railgun: RailgunProvider,
    /// The provider's in-memory state (a clone of the handle given to the builder): what
    /// [`Session::persist_state`] exports.
    pub state: RailgunState,
    pub signer: Arc<dyn RailgunSigner>,
    pub address: RailgunAddress,
    pub mode: &'static str,
    pub store: Box<dyn Store>,
    pub squid: Squid,
    pub tokens: HashMap<String, TokenMeta>,
    pub commitment_refs: HashMap<String, ChainRef>,
    pub nullifier_refs: HashMap<String, ChainRef>,
    /// blinded commitment -> list -> status, refreshed at every sync except for `Valid`.
    pub poi_cache: PoiCache,
    /// railgun txid -> subsquid `Transaction` record (chain hash, timestamp, unshield preimage).
    pub op_refs: HashMap<String, OpRef>,
    /// railgun txid -> operation spending our notes found from the nullifiers (not retained by the
    /// txid indexer).
    pub chain_ops: HashMap<String, OpIn>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: ChainConfig,
        provider: DynProvider,
        railgun: RailgunProvider,
        state: RailgunState,
        signer: Arc<dyn RailgunSigner>,
        mode: &'static str,
        store: Box<dyn Store>,
        caches: Caches,
    ) -> Self {
        let address = signer.address();
        let squid = Squid::new(chain.subsquid_endpoint.clone());
        Session {
            chain,
            provider,
            railgun,
            state,
            signer,
            address,
            mode,
            store,
            squid,
            tokens: HashMap::new(),
            commitment_refs: caches.commitment_refs,
            nullifier_refs: caches.nullifier_refs,
            poi_cache: caches.poi_cache,
            op_refs: caches.op_refs,
            chain_ops: caches.chain_ops,
        }
    }

    pub fn cache_value(&self) -> Value {
        serde_json::json!({
            "commitments": self.commitment_refs,
            "nullifiers": self.nullifier_refs,
            "poi": self.poi_cache,
            "ops": self.op_refs,
            "chainOps": self.chain_ops,
        })
    }

    /// Saves the viewer cache, and the SDK state with it: the cache is written after a refresh,
    /// which follows every operation that can change the state. A failed state save is only
    /// logged here; the engines report the one they do after a sync.
    pub async fn persist(&self) {
        self.store.save_cache(self.cache_value());
        if let Err(e) = self.persist_state().await {
            tracing::warn!("Railgun state not saved: {e:#}");
        }
    }

    /// Exports the SDK state and hands it to the store. The in-memory state is ahead of the saved
    /// one until the next successful save; the SDK replays nothing, so what was not saved is
    /// synced again at the next unlock.
    pub async fn persist_state(&self) -> Result<()> {
        let bytes = state_bytes(&self.state)?;
        self.store.save_state(bytes).await.context("saving the Railgun state")
    }
}

pub fn hex64(v: U256) -> String {
    format!("0x{v:064x}")
}

pub fn note_in(n: &UtxoNote, spent: bool) -> NoteIn {
    // `U256::from` resolves to ruint's inherent `UintTryFrom` method; go through the trait impl.
    let hash: U256 = <U256 as From<_>>::from(n.hash);
    let token = match n.asset {
        AssetId::Erc20(a) => Some(format!("{a:?}")),
        #[allow(unreachable_patterns)]
        _ => None,
    };
    NoteIn {
        hash: hex64(hash),
        tree: n.tree_number,
        leaf: n.leaf_index,
        value: n.value,
        token,
        token_hash: hex64(n.asset.hash()),
        memo: n.memo.clone(),
        commitment_type: serde_json::to_value(n.commitment_type)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", n.commitment_type)),
        npk: hex64(n.note_public_key),
        random: format!("0x{}", hex::encode(n.random)),
        nullifier: hex64(n.nullifier),
        blinded: hex64(n.blinded_commitment),
        spent,
    }
}

pub fn sent_in(s: &SentNote) -> SentIn {
    SentIn {
        hash: hex64(s.hash),
        tree: s.tree_number,
        leaf: s.leaf_index,
        value: s.value,
        token_hash: hex64(s.token_hash),
        npk: hex64(s.note_public_key),
        blinded: hex64(blinded_commitment(s.hash, s.note_public_key, s.tree_number, s.leaf_index)),
    }
}

/// Operations spending our notes that the txid indexer did not retain (POI off, a txid sync gap,
/// an unreachable POI node): the viewing key gives every nullifier, the subsquid the operation
/// that published it (looked up by nullifier, then the full `Transaction` of that block). Kept in
/// the viewer cache.
async fn add_chain_ops(session: &mut Session, notes: &[NoteIn], ops: &mut Vec<OpIn>) {
    let covered: std::collections::HashSet<String> =
        ops.iter().flat_map(|o| o.nullifiers.iter().cloned()).collect();
    let mut uncovered: Vec<String> = notes
        .iter()
        .filter(|n| n.spent && !covered.contains(&n.nullifier))
        .map(|n| n.nullifier.clone())
        .collect();
    uncovered.sort();
    uncovered.dedup();
    if uncovered.is_empty() {
        return;
    }
    let mut known: std::collections::HashSet<String> = ops.iter().map(|o| o.railgun_txid.clone()).collect();
    for op in session.chain_ops.values() {
        if op.nullifiers.iter().any(|n| uncovered.contains(n)) && known.insert(op.railgun_txid.clone()) {
            ops.push(op.clone());
        }
    }
    let covered: std::collections::HashSet<String> =
        ops.iter().flat_map(|o| o.nullifiers.iter().cloned()).collect();
    let rest: Vec<String> = uncovered.into_iter().filter(|n| !covered.contains(n)).collect();
    if rest.is_empty() {
        return;
    }
    let ask: Vec<String> = rest.iter().filter(|n| !session.nullifier_refs.contains_key(*n)).cloned().collect();
    if !ask.is_empty() {
        let found = session.squid.nullifiers_by_value(&ask).await;
        session.nullifier_refs.extend(found);
    }
    let mut blocks: Vec<u64> = rest
        .iter()
        .filter_map(|n| session.nullifier_refs.get(n))
        .map(|r| r.block_number)
        .collect();
    blocks.sort_unstable();
    blocks.dedup();
    if blocks.is_empty() {
        tracing::warn!("{} spent note(s) without a known spending operation", rest.len());
        return;
    }
    let parse = |h: &String| U256::from_str_radix(h.trim_start_matches("0x"), 16).ok();
    let mut added = 0usize;
    for c in session.squid.operations_at(&blocks).await {
        if !c.nullifiers.iter().any(|n| rest.contains(n)) {
            continue;
        }
        let (Some(nfs), Some(cms), Some(bph)) = (
            c.nullifiers.iter().map(parse).collect::<Option<Vec<_>>>(),
            c.commitments.iter().map(parse).collect::<Option<Vec<_>>>(),
            parse(&c.bound_params_hash),
        ) else {
            continue;
        };
        let rid = railgun_txid_hex(&nfs, &cms, bph);
        let cref = ChainRef {
            block_number: c.block_number,
            timestamp: c.timestamp,
            transaction_hash: Some(c.transaction_hash.clone()),
        };
        for n in &c.nullifiers {
            session.nullifier_refs.entry(n.clone()).or_insert_with(|| cref.clone());
        }
        for h in &c.commitments {
            session.commitment_refs.entry(h.clone()).or_insert_with(|| cref.clone());
        }
        let op = OpIn {
            railgun_txid: rid.clone(),
            block_number: c.block_number,
            nullifiers: c.nullifiers,
            commitments: c.commitments,
            bound_params_hash: c.bound_params_hash,
            utxo_tree_in: c.utxo_tree_in,
            utxo_tree_out: c.utxo_tree_out,
            utxo_out_start_index: c.utxo_out_start_index,
            has_unshield: c.has_unshield,
            from_chain: true,
        };
        session.chain_ops.insert(rid.clone(), op.clone());
        if known.insert(rid) {
            ops.push(op);
            added += 1;
        }
    }
    if added > 0 {
        tracing::info!("{added} operation(s) spending our notes found from their nullifiers");
    }
}

/// POI status of one blinded commitment on each list, as the UI shows it.
fn poi_lists(session: &Session, blinded: U256) -> std::collections::BTreeMap<String, String> {
    session
        .railgun
        .poi_status_per_list(blinded)
        .into_iter()
        .map(|(list, status)| {
            let s = serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "Unknown".into());
            (list, s)
        })
        .collect()
}

pub async fn build_history(session: &mut Session) -> Result<history::History> {
    let state = session
        .railgun
        .account_state(session.address)
        .ok_or_else(|| anyhow!("account not registered"))?;

    let mut notes: Vec<NoteIn> = state.notes.iter().map(|n| note_in(n, false)).collect();
    notes.extend(state.spent.iter().map(|n| note_in(n, true)));
    let sent: Vec<SentIn> = state.sent.iter().map(sent_in).collect();

    let mut ops: Vec<OpIn> = session
        .railgun
        .own_operations()
        .into_iter()
        .map(|(rid, op)| OpIn {
            railgun_txid: rid,
            block_number: op.block_number,
            nullifiers: op.nullifiers.iter().map(|n| hex64(*n)).collect(),
            commitments: op.commitment_hashes.iter().map(|c| hex64(*c)).collect(),
            bound_params_hash: hex64(op.bound_params_hash),
            utxo_tree_in: op.utxo_tree_in,
            utxo_tree_out: op.utxo_tree_out,
            utxo_out_start_index: op.utxo_out_start_index,
            has_unshield: op.has_unshield,
            from_chain: false,
        })
        .collect();
    add_chain_ops(session, &notes, &mut ops).await;

    // POI statuses per list, read from the SDK's local copy of the POI lists: no request names
    // a commitment, so every note can be looked up at every refresh.
    let mut statuses: HashMap<String, std::collections::BTreeMap<String, String>> = HashMap::new();
    if session.railgun.poi_enabled() {
        let targets = state
            .notes
            .iter()
            .chain(state.spent.iter())
            .map(|n| n.blinded_commitment)
            .chain(state.sent.iter().map(|s| {
                blinded_commitment(s.hash, s.note_public_key, s.tree_number, s.leaf_index)
            }));
        for blinded in targets {
            statuses.insert(hex64(blinded), poi_lists(session, blinded));
        }
    }
    let pending: Vec<Value> = session
        .railgun
        .poi_pending()
        .into_iter()
        .filter_map(|p| serde_json::to_value(p).ok())
        .collect();
    let list_keys = session.railgun.poi_list_keys();

    // Token addresses by token hash, and metadata.
    let mut tokens_by_hash: HashMap<String, String> = HashMap::new();
    for n in &notes {
        if let Some(t) = &n.token {
            tokens_by_hash.insert(n.token_hash.clone(), t.to_ascii_lowercase());
        }
    }
    let mut wanted: Vec<String> = tokens_by_hash.values().cloned().collect();
    wanted.push(format!("{:?}", session.chain.wrapped_base_token));
    wanted.sort();
    wanted.dedup();
    for t in wanted {
        if !session.tokens.contains_key(&t)
            && let Ok(addr) = t.parse::<Address>()
        {
            let meta = chain::token_meta(&session.provider, addr).await;
            session.tokens.insert(t.clone(), meta);
        }
    }

    // Operations by block (subsquid `Transaction`): chain hash, timestamp, unshield preimage.
    let pending_ops: Vec<&OpIn> = ops
        .iter()
        .filter(|op| !session.op_refs.contains_key(&op.railgun_txid))
        .collect();
    let mut blocks: Vec<u64> = pending_ops.iter().map(|op| op.block_number).collect();
    blocks.sort_unstable();
    blocks.dedup();
    if !blocks.is_empty() {
        let found = session.squid.transactions_at(&blocks).await;
        for op in pending_ops {
            let hit = found.iter().find(|r| {
                r.block_number == op.block_number
                    && op.nullifiers.iter().any(|n| r.nullifiers.contains(n))
            });
            match hit {
                Some(r) if !(op.has_unshield && r.unshield_to.is_none()) => {
                    session.op_refs.insert(op.railgun_txid.clone(), r.clone());
                }
                _ => {
                    // subsquid gave nothing usable: read the contract logs of that block.
                    if let Some(r) = chain::op_from_logs(
                        &session.provider,
                        session.chain.railgun_smart_wallet,
                        op.block_number,
                        &op.nullifiers,
                    )
                    .await
                    {
                        session.op_refs.insert(op.railgun_txid.clone(), r);
                    } else if let Some(r) = hit {
                        session.op_refs.insert(op.railgun_txid.clone(), r.clone());
                    }
                }
            }
        }
    }
    for op in &ops {
        if let Some(r) = session.op_refs.get(&op.railgun_txid) {
            let cref = ChainRef {
                block_number: r.block_number,
                timestamp: r.timestamp,
                transaction_hash: Some(r.transaction_hash.clone()),
            };
            for n in &op.nullifiers {
                session.nullifier_refs.entry(n.clone()).or_insert_with(|| cref.clone());
            }
            for c in &op.commitments {
                session.commitment_refs.entry(c.clone()).or_insert_with(|| cref.clone());
            }
        }
    }

    // Chain references (subsquid), kept in the viewer cache.
    let mut missing: Vec<String> = notes
        .iter()
        .map(|n| n.hash.clone())
        .chain(sent.iter().map(|s| s.hash.clone()))
        .filter(|h| !session.commitment_refs.contains_key(h))
        .collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        let found = session.squid.commitments(&missing).await;
        session.commitment_refs.extend(found);
    }
    // nullifiers still without a reference: ask by block (blocks of the operations that spent them)
    let mut nf_blocks: Vec<u64> = ops
        .iter()
        .filter(|op| op.nullifiers.iter().any(|n| !session.nullifier_refs.contains_key(n)))
        .map(|op| op.block_number)
        .collect();
    nf_blocks.sort_unstable();
    nf_blocks.dedup();
    if !nf_blocks.is_empty() {
        let found = session.squid.nullifiers_at(&nf_blocks).await;
        for (k, v) in found {
            session.nullifier_refs.entry(k).or_insert(v);
        }
    }
    session.persist().await;

    let mut un_blocks: Vec<u64> = ops.iter().filter(|op| op.has_unshield).map(|op| op.block_number).collect();
    un_blocks.sort_unstable();
    un_blocks.dedup();
    let mut unshields = if un_blocks.is_empty() {
        vec![]
    } else {
        session.squid.unshields_at(&un_blocks).await
    };
    for op in ops.iter().filter(|op| op.has_unshield) {
        let Some(r) = session.op_refs.get(&op.railgun_txid) else { continue };
        if unshields.iter().any(|u| u.transaction_hash == r.transaction_hash) {
            continue;
        }
        if let (Some(to), Some(value)) = (&r.unshield_to, &r.unshield_value) {
            // gross preimage value; the event lookup (fee split) was not available
            unshields.push(UnshieldRef {
                transaction_hash: r.transaction_hash.clone(),
                block_number: r.block_number,
                timestamp: r.timestamp,
                to: to.clone(),
                token_address: r.unshield_token.clone(),
                amount: value.clone(),
                fee: "0".into(),
                event_log_index: None,
            });
        }
    }
    let mut token_meta = session.tokens.clone();
    for u in &unshields {
        if let Some(t) = &u.token_address
            && let Entry::Vacant(e) = token_meta.entry(t.to_ascii_lowercase())
            && let Ok(addr) = e.key().parse::<Address>()
        {
            let meta = chain::token_meta(&session.provider, addr).await;
            session.tokens.insert(e.key().clone(), meta.clone());
            e.insert(meta);
        }
    }

    // Unshield POI: the community engine keys it by the railgun txid itself
    // (`BlindedCommitment.getForUnshield(railgunTxid)`). No preimage needed.
    let mut unshield_statuses: HashMap<String, std::collections::BTreeMap<String, String>> = HashMap::new();
    if session.railgun.poi_enabled() {
        let decrypted: std::collections::HashSet<String> = notes
            .iter()
            .map(|n| n.hash.clone())
            .chain(sent.iter().map(|s| s.hash.clone()))
            .collect();
        for op in ops.iter().filter(|op| {
            op.has_unshield || !op.commitments.iter().any(|c| decrypted.contains(c))
        }) {
            let Ok(rid) = U256::from_str_radix(op.railgun_txid.trim_start_matches("0x"), 16) else {
                continue;
            };
            let map = poi_lists(session, rid);
            if !map.is_empty() {
                unshield_statuses.insert(op.railgun_txid.clone(), map);
            }
        }
    }
    let recovered_valid: std::collections::HashSet<String> =
        session.railgun.poi_recovered_valid().into_iter().collect();

    let input = history::Input {
        address: session.address.to_string(),
        chain_id: session.chain.id,
        explorer: explorer(session.chain.id),
        list_keys,
        notes,
        sent,
        ops,
        statuses,
        pending,
        tokens_by_hash,
        token_meta,
        commitment_refs: session.commitment_refs.clone(),
        nullifier_refs: session.nullifier_refs.clone(),
        unshields,
        unshield_statuses,
        recovered_valid,
    };
    let _ = &session.signer;
    let _ = session.mode;
    let h = history::build(&input);
    for t in h.transactions.iter().filter(|t| t.emitted) {
        tracing::info!(
            "tx {} inputs {}/{} outputs {}/{} unshield {:?} pois {:?} submitted {} pending {}",
            &t.id[..12.min(t.id.len())],
            t.inputs.len(),
            t.inputs.len() + t.unknown_inputs,
            t.outputs.len(),
            t.outputs.len() + t.unknown_outputs,
            input.unshield_statuses.get(&t.id).map(|m| m.values().cloned().collect::<Vec<_>>()),
            t.pois,
            t.poi_submitted,
            t.poi_pending
        );
    }
    Ok(h)
}

/// Railgun txid (`0x` + 64 hex digits) of an operation read from an indexer (subsquid
/// `Transaction`), for operations the txid indexer did not retain.
fn railgun_txid_hex(
    nullifiers: &[ruint::aliases::U256],
    commitments: &[ruint::aliases::U256],
    bound_params_hash: ruint::aliases::U256,
) -> String {
    let txid: ruint::aliases::U256 =
        railgun::crypto::railgun_txid::Txid::new(nullifiers, commitments, bound_params_hash).into();
    format!("0x{txid:064x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_viewing_key_is_refused() {
        let p: UnlockParams = serde_json::from_value(serde_json::json!({
            "chainId": 11155111,
            "viewingKey": format!("0x{}", "11".repeat(32)),
        }))
        .unwrap();
        assert!(p.precheck().unwrap_err().to_string().contains("0zk address"));
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(42_000), "42 s");
        assert_eq!(fmt_duration(185_000), "3 min 05 s");
        assert_eq!(fmt_duration(3_900_000), "1 h 05 min");
    }

    #[test]
    fn state_round_trips_through_postcard() {
        let state = RailgunState::default();
        let bytes = state_bytes(&state).unwrap();
        let restored = state_from_bytes(&bytes).unwrap();
        assert_eq!(state_bytes(&restored).unwrap(), bytes);
    }

    #[test]
    fn corrupt_state_is_refused() {
        let Err(err) = state_from_bytes(&[0xff; 7]) else {
            panic!("garbage decoded as a Railgun state");
        };
        assert!(err.to_string().contains("Railgun state"), "{err:#}");
    }
}

//! Session core shared by the native daemon (`engine.rs`) and the web build
//! (`railgun-viewer-web`): unlock parameters, signer resolution, the per-account session and the
//! history snapshot. No thread, no file, no clock of its own: persistence goes through
//! [`CacheStore`], progress and log lines through [`SharedRef`].

use std::{collections::HashMap, sync::Arc};

use alloy::{
    primitives::{Address, U256},
    providers::DynProvider,
};
use anyhow::{Result, anyhow, bail};
use railgun::{
    account::{
        address::RailgunAddress,
        signer::{PrivateKeySigner as RgSigner, RailgunSigner},
    },
    builder::RailgunBuilder,
    caip::AssetId,
    chain_config::ChainConfig,
    provider::{CommitmentKind, RailgunProvider, SentNote, UtxoNote, blinded_commitment},
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    chain::{self, ChainRef, OpRef, Squid, TokenMeta, UnshieldRef},
    history::{self, NoteIn, OpIn, SentIn},
    keys::{self, Credentials, Resolved},
    shared::{SharedRef, log, now_ms},
    signer::MasterSigner,
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
        }
    }

    /// Checks done before replying to the unlock request: key syntax, and the 0zk address
    /// against the viewing key when both are given.
    pub fn precheck(&self) -> Result<()> {
        let r = keys::resolve(&self.credentials())?;
        if let (Resolved::ViewOnly { viewing }, Some(a)) = (&r, keys::non_empty(&self.address)) {
            keys::master_from_address(a, viewing)?;
        }
        Ok(())
    }

    /// Lengths of what was received, for the log when the request is refused.
    pub fn lengths(&self) -> String {
        format!(
            "unlock: received mnemonic={} viewingKey={} address={} (lengths)",
            self.mnemonic.as_deref().map(str::len).unwrap_or(0),
            self.viewing_key.as_deref().map(str::len).unwrap_or(0),
            self.address.as_deref().map(str::len).unwrap_or(0)
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

fn set_stage(shared: &SharedRef, stage: Option<&'static str>) {
    if let Ok(mut s) = shared.lock() {
        s.stage = stage;
        s.updated_at = now_ms();
    }
}

/// Signer for the unlock request: full keys from a mnemonic, else a view-only signer whose master
/// public key is read from the 0zk address or, without it, discovered on chain (full scan).
pub async fn make_signer(
    resolved: Resolved,
    address: Option<&str>,
    chain: &ChainConfig,
    provider: &DynProvider,
    shared: &SharedRef,
) -> Result<(Arc<dyn RailgunSigner>, &'static str)> {
    match resolved {
        Resolved::Full { spending, viewing, scheme } => Ok((RgSigner::new_evm(spending, viewing, chain.id), scheme)),
        Resolved::ViewOnly { viewing } if address.is_some() => {
            let master = keys::master_from_address(address.unwrap_or_default(), &viewing)?;
            log(shared, "✓ master public key read from the 0zk address (viewing key checked against it)");
            Ok((MasterSigner::new(viewing, master, chain.id), "n/a"))
        }
        Resolved::ViewOnly { viewing } => {
            // The master public key is not derivable from the viewing key: read it from the
            // first transact note received in clear. Full scan on the first run only.
            set_stage(shared, Some("discovering master key"));
            log(shared, format!("view-only: looking for the master public key on chain (first transact note received), scanning from block {}…", chain.deployment_block));
            let sh = shared.clone();
            let t0 = now_ms();
            let progress = move |to: u64, head: u64| {
                if let Ok(mut s) = sh.lock() {
                    s.stage_detail = Some(format!("block {to} / {head}"));
                    s.updated_at = now_ms();
                }
                log(&sh, format!("view-only: scanned up to block {to} / {head} ({:.0} s)", (now_ms() - t0) as f64 / 1000.0));
            };
            let scan = RailgunBuilder::new(chain.clone(), provider.clone());
            let found = scan
                .discover_master_key(viewing, None, &progress)
                .await
                .map_err(|e| anyhow!("master key discovery: {e}"))?;
            set_stage(shared, Some("unlocking"));
            let Some((master, ts)) = found else {
                bail!(
                    "no transact note received in clear for this viewing key: the master public \
                     key cannot be inferred (account with shields only, or senders that revealed \
                     themselves). Provide the 0zk address with the viewing key."
                );
            };
            log(shared, format!("✓ master public key found (note received at timestamp {ts})"));
            Ok((MasterSigner::new(viewing, master, chain.id), "n/a"))
        }
    }
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

/// Where the viewer cache (chain references, POI statuses, operations) is persisted: a JSON file
/// next to the database on the daemon, IndexedDB in the web build.
pub trait CacheStore {
    fn save(&self, value: Value);
}

pub type PoiCache = HashMap<String, std::collections::BTreeMap<String, String>>;

/// Viewer cache, kept outside the SDK database.
#[derive(Default)]
pub struct Caches {
    pub commitment_refs: HashMap<String, ChainRef>,
    pub nullifier_refs: HashMap<String, ChainRef>,
    pub poi_cache: PoiCache,
    pub op_refs: HashMap<String, OpRef>,
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
        }
    }
}

pub struct Session {
    pub chain: ChainConfig,
    pub provider: DynProvider,
    pub railgun: RailgunProvider,
    pub signer: Arc<dyn RailgunSigner>,
    pub address: RailgunAddress,
    pub mode: &'static str,
    pub store: Box<dyn CacheStore>,
    pub squid: Squid,
    pub tokens: HashMap<String, TokenMeta>,
    pub commitment_refs: HashMap<String, ChainRef>,
    pub nullifier_refs: HashMap<String, ChainRef>,
    /// blinded commitment -> list -> status, refreshed at every sync except for `Valid`.
    pub poi_cache: PoiCache,
    /// railgun txid -> subsquid `Transaction` record (chain hash, timestamp, unshield preimage).
    pub op_refs: HashMap<String, OpRef>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: ChainConfig,
        provider: DynProvider,
        railgun: RailgunProvider,
        signer: Arc<dyn RailgunSigner>,
        mode: &'static str,
        store: Box<dyn CacheStore>,
        caches: Caches,
    ) -> Self {
        let address = signer.address();
        let squid = Squid::new(chain.subsquid_endpoint.clone());
        Session {
            chain,
            provider,
            railgun,
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
        }
    }

    pub fn cache_value(&self) -> Value {
        serde_json::json!({
            "commitments": self.commitment_refs,
            "nullifiers": self.nullifier_refs,
            "poi": self.poi_cache,
            "ops": self.op_refs,
        })
    }

    pub fn persist(&self) {
        self.store.save(self.cache_value());
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
        commitment_type: serde_json::to_value(&n.commitment_type)
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

pub async fn build_history(session: &mut Session) -> Result<history::History> {
    let state = session
        .railgun
        .account_state(session.address)
        .ok_or_else(|| anyhow!("account not registered"))?;

    let mut notes: Vec<NoteIn> = state.notes.iter().map(|n| note_in(n, false)).collect();
    notes.extend(state.spent.iter().map(|n| note_in(n, true)));
    let sent: Vec<SentIn> = state.sent.iter().map(sent_in).collect();

    let ops: Vec<OpIn> = session
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
        })
        .collect();

    // POI statuses: the SDK only caches statuses it fetched merkle proofs for, so every note is
    // asked to the node here (per list), `Valid` answers being final and kept in the cache.
    let mut statuses: HashMap<String, std::collections::BTreeMap<String, String>> = HashMap::new();
    for (blinded, lists) in session.railgun.poi_statuses() {
        let entry = statuses.entry(blinded).or_default();
        for (list, status) in lists {
            let s = status
                .and_then(|st| serde_json::to_value(st).ok())
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "Unknown".into());
            entry.insert(list, s);
        }
    }
    if session.railgun.poi_enabled() {
        let mut targets: Vec<(U256, CommitmentKind)> = Vec::new();
        for n in state.notes.iter().chain(state.spent.iter()) {
            targets.push((n.blinded_commitment, n.commitment_type.clone()));
        }
        for s in &state.sent {
            targets.push((
                blinded_commitment(s.hash, s.note_public_key, s.tree_number, s.leaf_index),
                CommitmentKind::Transact,
            ));
        }
        let mut probed = 0usize;
        for (blinded, ctype) in targets {
            let key = hex64(blinded);
            let cached = session.poi_cache.get(&key);
            let all_valid = cached
                .map(|m| !m.is_empty() && m.values().all(|v| v == "Valid"))
                .unwrap_or(false);
            if all_valid {
                statuses.insert(key, cached.cloned().unwrap_or_default());
                continue;
            }
            let lists = session.railgun.probe_poi(blinded, ctype).await;
            probed += 1;
            let entry = statuses.entry(key.clone()).or_default();
            for (list, status) in lists {
                let s = status
                    .and_then(|st| serde_json::to_value(st).ok())
                    .and_then(|v| v.as_str().map(str::to_owned));
                match s {
                    Some(s) => {
                        entry.insert(list, s);
                    }
                    None => {
                        // unreachable list: keep what the cache had, else Unknown
                        if let Some(prev) = cached.and_then(|m| m.get(&list)).cloned() {
                            entry.insert(list, prev);
                        }
                    }
                }
            }
            session.poi_cache.insert(key, entry.clone());
        }
        if probed > 0 {
            tracing::info!("probed POI statuses of {probed} note(s)");
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
        if !session.tokens.contains_key(&t) {
            if let Ok(addr) = t.parse::<Address>() {
                let meta = chain::token_meta(&session.provider, addr).await;
                session.tokens.insert(t.clone(), meta);
            }
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

    // Chain references (subsquid), cached on disk.
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
    session.persist();

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
        if let Some(t) = &u.token_address {
            let t = t.to_ascii_lowercase();
            if !token_meta.contains_key(&t) {
                if let Ok(addr) = t.parse::<Address>() {
                    let meta = chain::token_meta(&session.provider, addr).await;
                    session.tokens.insert(t.clone(), meta.clone());
                    token_meta.insert(t, meta);
                }
            }
        }
    }

    // Unshield POI: the community engine keys it by the railgun txid itself
    // (`BlindedCommitment.getForUnshield(railgunTxid)`), type Unshield. No preimage needed.
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
            let key = format!("unshield:{}", op.railgun_txid);
            let cached = session.poi_cache.get(&key).cloned();
            let all_valid = cached
                .as_ref()
                .map(|m| !m.is_empty() && m.values().all(|v| v == "Valid"))
                .unwrap_or(false);
            let map = if all_valid {
                cached.unwrap_or_default()
            } else {
                let mut m = cached.unwrap_or_default();
                for (list, status) in session.railgun.probe_poi(rid, CommitmentKind::Unshield).await {
                    if let Some(st) = status
                        .and_then(|st| serde_json::to_value(st).ok())
                        .and_then(|v| v.as_str().map(str::to_owned))
                    {
                        m.insert(list, st);
                    }
                }
                session.poi_cache.insert(key, m.clone());
                m
            };
            if !map.is_empty() {
                unshield_statuses.insert(op.railgun_txid.clone(), map);
            }
        }
        session.persist();
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

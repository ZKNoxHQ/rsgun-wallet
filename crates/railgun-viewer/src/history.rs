//! Read model: transactions and notes of one account, built from the SDK account state, the
//! operations kept by the txid indexer, the POI statuses and the chain references.
//!
//! Pure functions over plain data so they can be unit-tested without an engine.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;
use serde_json::{Value, json};

use crate::chain::{ChainRef, TokenMeta, UnshieldRef};

/// One note as seen by this account (received, spent or not).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteIn {
    pub hash: String,
    pub tree: u32,
    pub leaf: u32,
    pub value: u128,
    pub token: Option<String>,
    pub token_hash: String,
    pub memo: String,
    pub commitment_type: String,
    pub npk: String,
    pub random: String,
    pub nullifier: String,
    pub blinded: String,
    pub spent: bool,
}

/// One output this account created for somebody else.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SentIn {
    pub hash: String,
    pub tree: u32,
    pub leaf: u32,
    pub value: u128,
    pub token_hash: String,
    pub npk: String,
    pub blinded: String,
}

/// One of our own operations, as the txid indexer keeps it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpIn {
    pub railgun_txid: String,
    pub block_number: u64,
    pub nullifiers: Vec<String>,
    pub commitments: Vec<String>,
    pub bound_params_hash: String,
    pub utxo_tree_in: u32,
    pub utxo_tree_out: u32,
    pub utxo_out_start_index: u32,
    pub has_unshield: bool,
}

pub struct Input {
    pub address: String,
    pub chain_id: u64,
    pub explorer: String,
    pub list_keys: Vec<String>,
    pub notes: Vec<NoteIn>,
    pub sent: Vec<SentIn>,
    pub ops: Vec<OpIn>,
    /// blinded commitment -> list -> status ("Valid", "ProofSubmitted", "Missing", "ShieldBlocked")
    pub statuses: HashMap<String, BTreeMap<String, String>>,
    pub pending: Vec<Value>,
    /// token hash -> token address
    pub tokens_by_hash: HashMap<String, String>,
    pub token_meta: HashMap<String, TokenMeta>,
    pub commitment_refs: HashMap<String, ChainRef>,
    pub nullifier_refs: HashMap<String, ChainRef>,
    pub unshields: Vec<UnshieldRef>,
    /// railgun txid -> list -> status of the unshield output (probed with its own blinded commitment)
    pub unshield_statuses: HashMap<String, BTreeMap<String, String>>,
    /// railgun txids the SDK recovery pass saw as `Valid` on the node
    pub recovered_valid: HashSet<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteView {
    pub hash: String,
    pub tree: u32,
    pub leaf: u32,
    pub global_position: u64,
    pub value: String,
    pub token: Option<String>,
    pub symbol: String,
    pub decimals: u8,
    pub memo: String,
    /// shield | received | change | sent
    pub origin: &'static str,
    pub mine: bool,
    pub spent: bool,
    pub created_by: Option<String>,
    pub spent_by: Option<String>,
    pub commitment_type: String,
    pub blinded: String,
    pub npk: String,
    pub random: Option<String>,
    pub nullifier: Option<String>,
    pub pois: BTreeMap<String, String>,
    pub block_number: Option<u64>,
    pub timestamp: Option<u64>,
    pub transaction_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnshieldView {
    pub to: String,
    pub token: Option<String>,
    pub symbol: String,
    pub decimals: u8,
    pub amount: String,
    pub fee: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxView {
    pub id: String,
    /// shield | transact | unshield | receive
    pub kind: &'static str,
    pub emitted: bool,
    pub railgun_txid: Option<String>,
    pub transaction_hash: Option<String>,
    pub block_number: Option<u64>,
    pub timestamp: Option<u64>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub unknown_inputs: usize,
    pub unknown_outputs: usize,
    pub unshields: Vec<UnshieldView>,
    pub memos: Vec<String>,
    /// per token: value in / out (decimal strings)
    pub totals: Vec<TxTotal>,
    /// per list: best status across our outputs
    pub pois: BTreeMap<String, String>,
    pub poi_submitted: bool,
    pub poi_missing_lists: Vec<String>,
    pub poi_pending: bool,
    /// What the POI verdict was built from (shown in the detail panel).
    pub debug: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxTotal {
    pub symbol: String,
    pub decimals: u8,
    pub value_in: String,
    pub value_out: String,
    pub unshield: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingPoi {
    pub tx_id: String,
    pub railgun_txid: Option<String>,
    pub transaction_hash: Option<String>,
    pub block_number: Option<u64>,
    pub timestamp: Option<u64>,
    pub lists: Vec<String>,
    pub reason: String,
    pub outputs: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    pub address: String,
    pub chain_id: u64,
    pub list_keys: Vec<String>,
    pub transactions: Vec<TxView>,
    pub notes: Vec<NoteView>,
    pub missing_poi: Vec<MissingPoi>,
    pub pending_poi: Vec<Value>,
    /// Snapshot in the NOXAKU `notes-snapshot` format, for the lineage graph.
    pub graph: Value,
}

const RANK: [&str; 4] = ["Valid", "ProofSubmitted", "Missing", "ShieldBlocked"];
fn rank(s: &str) -> usize {
    RANK.iter().position(|r| *r == s).unwrap_or(2)
}

fn meta<'a>(input: &'a Input, token: &Option<String>) -> (String, u8) {
    match token.as_deref().and_then(|t| input.token_meta.get(&t.to_ascii_lowercase())) {
        Some(m) => (m.symbol.clone(), m.decimals),
        None => (
            token
                .as_deref()
                .map(|t| format!("{}…{}", &t[..6.min(t.len())], &t[t.len().saturating_sub(4)..]))
                .unwrap_or_else(|| "?".into()),
            18,
        ),
    }
}

pub fn build(input: &Input) -> History {
    let mut by_nullifier: HashMap<&str, &OpIn> = HashMap::new();
    let mut by_commitment: HashMap<&str, &OpIn> = HashMap::new();
    for op in &input.ops {
        for n in &op.nullifiers {
            by_nullifier.insert(n.as_str(), op);
        }
        for c in &op.commitments {
            by_commitment.insert(c.as_str(), op);
        }
    }
    let statuses_of = |blinded: &str| -> BTreeMap<String, String> {
        let mut m: BTreeMap<String, String> = input
            .statuses
            .get(blinded)
            .cloned()
            .unwrap_or_default();
        for k in &input.list_keys {
            m.entry(k.clone()).or_insert_with(|| "Unknown".into());
        }
        m
    };
    let cref = |hash: &str| input.commitment_refs.get(hash);

    // ---- notes ----
    let mut notes: Vec<NoteView> = Vec::new();
    for n in &input.notes {
        let created_by = by_commitment.get(n.hash.as_str()).map(|op| op.railgun_txid.clone());
        let is_shield = n.commitment_type.eq_ignore_ascii_case("shield");
        let origin = if is_shield {
            "shield"
        } else if created_by.is_some() {
            "change"
        } else {
            "received"
        };
        let spent_by = if n.spent {
            by_nullifier.get(n.nullifier.as_str()).map(|op| op.railgun_txid.clone())
        } else {
            None
        };
        let (symbol, decimals) = meta(input, &n.token);
        let r = cref(&n.hash);
        notes.push(NoteView {
            hash: n.hash.clone(),
            tree: n.tree,
            leaf: n.leaf,
            global_position: u64::from(n.tree) * 65536 + u64::from(n.leaf),
            value: n.value.to_string(),
            token: n.token.clone(),
            symbol,
            decimals,
            memo: n.memo.clone(),
            origin,
            mine: true,
            spent: n.spent,
            created_by: created_by.or_else(|| {
                is_shield.then(|| {
                    r.and_then(|r| r.transaction_hash.clone())
                        .map(|t| format!("shield:{t}"))
                        .unwrap_or_else(|| format!("shield:{}", n.hash))
                })
            }),
            spent_by,
            commitment_type: n.commitment_type.clone(),
            blinded: n.blinded.clone(),
            npk: n.npk.clone(),
            random: Some(n.random.clone()),
            nullifier: Some(n.nullifier.clone()),
            pois: statuses_of(&n.blinded),
            block_number: r.map(|r| r.block_number),
            timestamp: r.and_then(|r| r.timestamp),
            transaction_hash: r.and_then(|r| r.transaction_hash.clone()),
        });
    }
    for s in &input.sent {
        let token = input.tokens_by_hash.get(&s.token_hash).cloned();
        let (symbol, decimals) = meta(input, &token);
        let r = cref(&s.hash);
        notes.push(NoteView {
            hash: s.hash.clone(),
            tree: s.tree,
            leaf: s.leaf,
            global_position: u64::from(s.tree) * 65536 + u64::from(s.leaf),
            value: s.value.to_string(),
            token,
            symbol,
            decimals,
            memo: String::new(),
            origin: "sent",
            mine: false,
            spent: false,
            created_by: by_commitment.get(s.hash.as_str()).map(|op| op.railgun_txid.clone()),
            spent_by: None,
            commitment_type: "Transact".into(),
            blinded: s.blinded.clone(),
            npk: s.npk.clone(),
            random: None,
            nullifier: None,
            pois: statuses_of(&s.blinded),
            block_number: r.map(|r| r.block_number),
            timestamp: r.and_then(|r| r.timestamp),
            transaction_hash: r.and_then(|r| r.transaction_hash.clone()),
        });
    }
    let note_by_hash: HashMap<&str, &NoteView> = notes.iter().map(|n| (n.hash.as_str(), n)).collect();

    // ---- transactions ----
    let mut txs: Vec<TxView> = Vec::new();
    let pending_txids: HashSet<String> = input
        .pending
        .iter()
        .filter_map(|p| p.get("txid").and_then(Value::as_str).map(|s| norm(s)))
        .collect();
    let unshields_by_tx: HashMap<String, Vec<&UnshieldRef>> = {
        let mut m: HashMap<String, Vec<&UnshieldRef>> = HashMap::new();
        for u in &input.unshields {
            m.entry(u.transaction_hash.to_ascii_lowercase()).or_default().push(u);
        }
        m
    };

    for op in &input.ops {
        let inputs: Vec<String> = op
            .nullifiers
            .iter()
            .filter_map(|nf| notes.iter().find(|n| n.nullifier.as_deref() == Some(nf.as_str())).map(|n| n.hash.clone()))
            .collect();
        let outputs: Vec<String> = op
            .commitments
            .iter()
            .filter(|c| note_by_hash.contains_key(c.as_str()))
            .cloned()
            .collect();
        let unknown_inputs = op.nullifiers.len() - inputs.len();
        let unknown_outputs = op.commitments.len() - outputs.len();
        let r = op
            .nullifiers
            .iter()
            .find_map(|nf| input.nullifier_refs.get(nf))
            .or_else(|| op.commitments.iter().find_map(|c| input.commitment_refs.get(c)));
        let transaction_hash = r.and_then(|r| r.transaction_hash.clone()).map(|t| t.to_ascii_lowercase());
        let timestamp = r.and_then(|r| r.timestamp);
        let unshields: Vec<UnshieldView> = transaction_hash
            .as_deref()
            .and_then(|t| unshields_by_tx.get(t))
            .map(|list| {
                list.iter()
                    .map(|u| {
                        let (symbol, decimals) = meta(input, &u.token_address);
                        UnshieldView {
                            to: u.to.clone(),
                            token: u.token_address.clone(),
                            symbol,
                            decimals,
                            amount: u.amount.clone(),
                            fee: u.fee.clone(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        // totals per symbol
        let mut totals: BTreeMap<String, (u8, u128, u128, u128)> = BTreeMap::new();
        for h in &inputs {
            let n = note_by_hash[h.as_str()];
            let e = totals.entry(n.symbol.clone()).or_insert((n.decimals, 0, 0, 0));
            e.1 += n.value.parse::<u128>().unwrap_or(0);
        }
        for h in &outputs {
            let n = note_by_hash[h.as_str()];
            let e = totals.entry(n.symbol.clone()).or_insert((n.decimals, 0, 0, 0));
            e.2 += n.value.parse::<u128>().unwrap_or(0);
        }
        for u in &unshields {
            let e = totals.entry(u.symbol.clone()).or_insert((u.decimals, 0, 0, 0));
            e.3 += u.amount.parse::<u128>().unwrap_or(0) + u.fee.parse::<u128>().unwrap_or(0);
        }
        // POI: best status per list over our outputs (change + sent)
        let mut pois: BTreeMap<String, String> = BTreeMap::new();
        for h in &outputs {
            for (list, st) in &note_by_hash[h.as_str()].pois {
                let cur = pois.entry(list.clone()).or_insert_with(|| "Unknown".into());
                if rank(st) < rank(cur) || cur == "Unknown" {
                    *cur = st.clone();
                }
            }
        }
        // the unshield output has its own blinded commitment on the node
        if let Some(m) = input.unshield_statuses.get(&op.railgun_txid) {
            for (list, st) in m {
                let cur = pois.entry(list.clone()).or_insert_with(|| "Unknown".into());
                if rank(st) < rank(cur) || cur == "Unknown" {
                    *cur = st.clone();
                }
            }
        }
        // the SDK recovery pass already saw this operation as Valid
        let recovered = input.recovered_valid.contains(&norm(&op.railgun_txid));
        if recovered {
            for k in &input.list_keys {
                let cur = pois.entry(k.clone()).or_insert_with(|| "Unknown".into());
                if cur != "Valid" {
                    *cur = "Valid".into();
                }
            }
            if input.list_keys.is_empty() && pois.is_empty() {
                pois.insert("*".into(), "Valid".into());
            }
        }
        for k in &input.list_keys {
            pois.entry(k.clone()).or_insert_with(|| "Unknown".into());
        }
        let poi_submitted = pois.values().any(|s| s == "Valid" || s == "ProofSubmitted");
        let debug = json!({
            "opNullifiers": op.nullifiers,
            "matchedInputs": inputs,
            "opCommitments": op.commitments,
            "matchedOutputs": outputs.iter().map(|h| {
                let n = note_by_hash[h.as_str()];
                json!({"hash": h, "origin": n.origin, "blinded": n.blinded, "pois": n.pois})
            }).collect::<Vec<_>>(),
            "unshieldStatuses": input.unshield_statuses.get(&op.railgun_txid),
            "recoveredValid": recovered,
            "listKeys": input.list_keys,
        });
        let poi_missing_lists: Vec<String> = pois
            .iter()
            .filter(|(_, s)| s.as_str() != "Valid" && s.as_str() != "ProofSubmitted")
            .map(|(l, _)| l.clone())
            .collect();
        let memos: Vec<String> = outputs
            .iter()
            .chain(inputs.iter())
            .filter_map(|h| note_by_hash.get(h.as_str()))
            .map(|n| n.memo.clone())
            .filter(|m| !m.is_empty())
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        txs.push(TxView {
            id: op.railgun_txid.clone(),
            kind: if op.has_unshield || !unshields.is_empty() { "unshield" } else { "transact" },
            emitted: !inputs.is_empty(),
            railgun_txid: Some(op.railgun_txid.clone()),
            transaction_hash,
            block_number: Some(op.block_number),
            timestamp,
            inputs,
            outputs,
            unknown_inputs,
            unknown_outputs,
            unshields,
            memos,
            totals: totals
                .into_iter()
                .map(|(symbol, (decimals, i, o, u))| TxTotal {
                    symbol,
                    decimals,
                    value_in: i.to_string(),
                    value_out: o.to_string(),
                    unshield: u.to_string(),
                })
                .collect(),
            pois,
            poi_submitted,
            poi_missing_lists,
            poi_pending: pending_txids.contains(&norm(&op.railgun_txid)),
            debug,
        });
    }
    // shields and third-party receipts: one entry per note without an own operation
    for n in &notes {
        if !n.mine {
            continue;
        }
        let owned_op = n.created_by.as_deref().map(|c| !c.starts_with("shield:")).unwrap_or(false);
        if owned_op {
            continue;
        }
        let is_shield = n.origin == "shield";
        let id = n
            .created_by
            .clone()
            .unwrap_or_else(|| format!("receive:{}", n.hash));
        if let Some(t) = txs.iter_mut().find(|t| t.id == id) {
            t.outputs.push(n.hash.clone());
            if !n.memo.is_empty() {
                t.memos.push(n.memo.clone());
            }
            if let Some(tot) = t.totals.iter_mut().find(|x| x.symbol == n.symbol) {
                let v = tot.value_out.parse::<u128>().unwrap_or(0) + n.value.parse::<u128>().unwrap_or(0);
                tot.value_out = v.to_string();
            } else {
                t.totals.push(TxTotal {
                    symbol: n.symbol.clone(),
                    decimals: n.decimals,
                    value_in: "0".into(),
                    value_out: n.value.clone(),
                    unshield: "0".into(),
                });
            }
            for (l, s) in &n.pois {
                let cur = t.pois.entry(l.clone()).or_insert_with(|| "Unknown".into());
                if rank(s) < rank(cur) || cur == "Unknown" {
                    *cur = s.clone();
                }
            }
            continue;
        }
        txs.push(TxView {
            id,
            kind: if is_shield { "shield" } else { "receive" },
            emitted: false,
            railgun_txid: None,
            transaction_hash: n.transaction_hash.clone(),
            block_number: n.block_number,
            timestamp: n.timestamp,
            inputs: vec![],
            outputs: vec![n.hash.clone()],
            unknown_inputs: 0,
            unknown_outputs: 0,
            unshields: vec![],
            memos: if n.memo.is_empty() { vec![] } else { vec![n.memo.clone()] },
            totals: vec![TxTotal {
                symbol: n.symbol.clone(),
                decimals: n.decimals,
                value_in: "0".into(),
                value_out: n.value.clone(),
                unshield: "0".into(),
            }],
            pois: n.pois.clone(),
            poi_submitted: n.pois.values().any(|s| s == "Valid" || s == "ProofSubmitted"),
            poi_missing_lists: vec![],
            poi_pending: false,
            debug: json!({"note": n.hash, "blinded": n.blinded, "pois": n.pois}),
        });
    }
    txs.sort_by(|a, b| {
        b.block_number
            .unwrap_or(0)
            .cmp(&a.block_number.unwrap_or(0))
            .then_with(|| b.timestamp.unwrap_or(0).cmp(&a.timestamp.unwrap_or(0)))
    });

    // ---- missing POI on emitted transactions ----
    let missing_poi: Vec<MissingPoi> = txs
        .iter()
        .filter(|t| t.emitted && !t.poi_submitted)
        .map(|t| MissingPoi {
            tx_id: t.id.clone(),
            railgun_txid: t.railgun_txid.clone(),
            transaction_hash: t.transaction_hash.clone(),
            block_number: t.block_number,
            timestamp: t.timestamp,
            lists: t.poi_missing_lists.clone(),
            reason: if t.poi_pending {
                "proof queued locally, waiting for the POI node to validate the txid".into()
            } else if t.outputs.is_empty() {
                "no decryptable output to prove (outputs to third parties only?)".into()
            } else if t.pois.values().all(|s| s == "Unknown") {
                "no status known: the node did not answer for any output (see debug)".into()
            } else {
                "no ProofSubmitted/Valid status on any output: proof never submitted".into()
            },
            outputs: t.outputs.clone(),
        })
        .collect();

    let graph = graph_snapshot(input, &notes, &txs);
    History {
        address: input.address.clone(),
        chain_id: input.chain_id,
        list_keys: input.list_keys.clone(),
        transactions: txs,
        notes,
        missing_poi,
        pending_poi: input.pending.clone(),
        graph,
    }
}

fn norm(s: &str) -> String {
    format!("0x{:0>64}", s.trim().trim_start_matches("0x").to_ascii_lowercase())
}

/// NOXAKU `notes-snapshot` v1, consumed unchanged by the lineage renderer.
fn graph_snapshot(input: &Input, notes: &[NoteView], txs: &[TxView]) -> Value {
    let tx_by_id: HashMap<&str, &TxView> = txs.iter().map(|t| (t.id.as_str(), t)).collect();
    let pois_json = |m: &BTreeMap<String, String>| -> Value {
        let filtered: BTreeMap<&String, &String> = m.iter().filter(|(_, v)| v.as_str() != "Unknown").collect();
        if filtered.is_empty() { Value::Null } else { json!(filtered) }
    };
    let chain_txid_of = |rid: &Option<String>| -> Option<String> {
        rid.as_deref()
            .and_then(|r| tx_by_id.get(r))
            .and_then(|t| t.transaction_hash.clone().or_else(|| t.railgun_txid.clone()))
    };
    let mut received = Vec::new();
    let mut sent = Vec::new();
    for n in notes {
        let base = json!({
            "commitment": n.hash, "npk": n.npk, "random": n.random, "value": n.value,
            "tokenAddress": n.token, "tokenType": 0, "tokenSubID": "0", "symbol": n.symbol,
            "decimals": n.decimals, "memoText": if n.memo.is_empty() { Value::Null } else { json!(n.memo) },
            "tree": n.tree, "position": n.leaf, "globalPosition": n.global_position,
            "blockNumber": n.block_number, "timestamp": n.timestamp,
            "txid": n.transaction_hash.clone().or_else(|| n.created_by.clone().filter(|c| !c.starts_with("shield:") && !c.starts_with("receive:"))),
            "blindedCommitment": n.blinded,
            "poisPerList": pois_json(&n.pois),
        });
        if n.mine {
            let mut r = base.clone();
            let created_rid = n.created_by.clone().filter(|c| !c.starts_with("shield:"));
            r["spendtxid"] = json!(chain_txid_of(&n.spent_by));
            r["nullifier"] = json!(n.nullifier);
            r["commitmentType"] = json!(if n.origin == "shield" { "ShieldCommitment" } else { "TransactCommitment" });
            r["transactCreationRailgunTxid"] = json!(created_rid);
            r["outputType"] = json!(if n.origin == "change" { Some(2) } else { None::<u8> });
            r["senderAddress"] = Value::Null;
            received.push(r);
            if n.origin == "change" {
                let mut s = base.clone();
                s["railgunTxid"] = json!(created_rid);
                s["outputType"] = json!(2);
                s["commitmentType"] = json!("TransactCommitment");
                sent.push(s);
            }
        } else {
            let mut s = base;
            s["railgunTxid"] = json!(n.created_by);
            s["outputType"] = json!(0);
            s["commitmentType"] = json!("TransactCommitment");
            sent.push(s);
        }
    }
    let mut railgun_txs = serde_json::Map::new();
    let mut unshields = Vec::new();
    for op in &input.ops {
        let t = tx_by_id.get(op.railgun_txid.as_str());
        let chain_txid = t.and_then(|t| t.transaction_hash.clone()).unwrap_or_else(|| op.railgun_txid.clone());
        let first_unshield = t.and_then(|t| t.unshields.first());
        railgun_txs.insert(
            op.railgun_txid.clone(),
            json!({
                "railgunTxid": op.railgun_txid, "txid": chain_txid, "blockNumber": op.block_number,
                "timestamp": t.and_then(|t| t.timestamp),
                "nullifiers": op.nullifiers, "commitments": op.commitments,
                "boundParamsHash": op.bound_params_hash, "utxoTreeIn": op.utxo_tree_in,
                "utxoTreeOut": op.utxo_tree_out, "utxoBatchStartPositionOut": op.utxo_out_start_index,
                "verificationHash": Value::Null, "graphID": Value::Null,
                "unshield": first_unshield.map(|u| json!({"toAddress": u.to, "value": u.amount, "tokenAddress": u.token, "tokenType": 0, "tokenSubID": "0"})),
            }),
        );
        if let Some(t) = t {
            for (i, u) in t.unshields.iter().enumerate() {
                unshields.push(json!({
                    "txid": chain_txid, "railgunTxid": op.railgun_txid, "blockNumber": op.block_number,
                    "timestamp": t.timestamp, "toAddress": u.to, "tokenAddress": u.token, "tokenType": 0,
                    "tokenSubID": "0", "symbol": u.symbol, "decimals": u.decimals, "amount": u.amount,
                    "fee": u.fee, "eventLogIndex": i, "poisPerList": pois_json(&t.pois),
                }));
            }
        }
    }
    json!({
        "format": "noxaku-notes-snapshot", "version": 1, "source": "railgun-viewer",
        "generatedAt": crate::shared::now_ms(), "chainId": input.chain_id,
        "explorer": input.explorer, "walletId": input.address, "listKeys": input.list_keys,
        "received": received, "sent": sent, "unshields": unshields, "railgunTxs": railgun_txs,
    })
}

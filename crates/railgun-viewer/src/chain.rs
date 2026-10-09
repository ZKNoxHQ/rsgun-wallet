//! Chain-side enrichment: block numbers, timestamps, transaction hashes and unshield details
//! from the Railgun subsquid (the SDK keeps none of them), plus ERC-20 metadata over RPC.

use std::collections::HashMap;

use alloy::{
    primitives::{Address, B256, U256},
    providers::{DynProvider, Provider},
    rpc::types::{BlockNumberOrTag, Filter, Log},
    sol,
    sol_types::SolEvent,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

sol! {
    #[sol(rpc)]
    contract ERC20 {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
    }
}

sol! {
    /// Railgun V2 `RailgunLogic` events needed here.
    struct RailgunTokenData {
        uint8 tokenType;
        address tokenAddress;
        uint256 tokenSubID;
    }
    event Unshield(address to, RailgunTokenData token, uint256 amount, uint256 fee);
}

/// RPC fallback for one operation: the Railgun contract logs of its block, the transaction whose
/// logs carry one of our nullifiers, and its `Unshield` events. Independent of the subsquid
/// schema; one `eth_getLogs` and one `eth_getBlockByNumber` per operation.
pub async fn op_from_logs(
    provider: &DynProvider,
    contract: Address,
    block: u64,
    nullifiers: &[String],
) -> Option<OpRef> {
    let nf_bytes: Vec<[u8; 32]> = nullifiers
        .iter()
        .filter_map(|n| {
            let b = hex::decode(n.trim_start_matches("0x")).ok()?;
            b.try_into().ok()
        })
        .collect();
    if nf_bytes.is_empty() {
        return None;
    }
    let filter = Filter::new().address(contract).from_block(block).to_block(block);
    let logs: Vec<Log> = match provider.get_logs(&filter).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("eth_getLogs at block {block} failed: {e}");
            return None;
        }
    };
    let mut by_tx: HashMap<B256, Vec<&Log>> = HashMap::new();
    for l in &logs {
        if let Some(h) = l.transaction_hash {
            by_tx.entry(h).or_default().push(l);
        }
    }
    let (tx_hash, tx_logs) = by_tx.iter().find(|(_, ls)| {
        ls.iter().any(|l| {
            let d: &[u8] = l.data().data.as_ref();
            nf_bytes.iter().any(|nb| d.windows(32).any(|w| w == nb))
        })
    })?;
    let mut op = OpRef {
        transaction_hash: format!("{tx_hash:#x}"),
        block_number: block,
        nullifiers: nullifiers.to_vec(),
        ..Default::default()
    };
    for l in tx_logs {
        if let Ok(ev) = Unshield::decode_log_data(l.data()) {
            op.has_unshield = true;
            op.unshield_to = Some(format!("{:#x}", ev.to));
            op.unshield_token = Some(format!("{:#x}", ev.token.tokenAddress));
            op.unshield_value = Some((ev.amount + ev.fee).to_string());
        }
    }
    op.timestamp = provider
        .get_block_by_number(BlockNumberOrTag::Number(block))
        .await
        .ok()
        .flatten()
        .map(|b| b.header.timestamp);
    Some(op)
}

#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenMeta {
    pub symbol: String,
    pub decimals: u8,
}

pub async fn token_meta(provider: &DynProvider, token: Address) -> TokenMeta {
    let erc20 = ERC20::new(token, provider.clone());
    let symbol = erc20.symbol().call().await.unwrap_or_else(|_| format!("{token:#x}"));
    let decimals = erc20.decimals().call().await.unwrap_or(18);
    TokenMeta { symbol, decimals }
}

/// Where a commitment or a nullifier landed on chain.
#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainRef {
    pub block_number: u64,
    pub timestamp: Option<u64>,
    pub transaction_hash: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnshieldRef {
    pub transaction_hash: String,
    pub block_number: u64,
    pub timestamp: Option<u64>,
    pub to: String,
    pub token_address: Option<String>,
    pub amount: String,
    pub fee: String,
    pub event_log_index: Option<u64>,
}

/// One Railgun operation as the subsquid `Transaction` entity records it. This is the same
/// entity the SDK's txid indexer reads; it also carries the chain hash, the timestamp and the
/// unshield preimage, which the SDK drops.
#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpRef {
    pub transaction_hash: String,
    pub block_number: u64,
    pub timestamp: Option<u64>,
    pub nullifiers: Vec<String>,
    pub commitments: Vec<String>,
    pub has_unshield: bool,
    pub unshield_to: Option<String>,
    pub unshield_token: Option<String>,
    /// gross value of the unshield preimage (amount + fee), decimal string
    pub unshield_value: Option<String>,
}

/// Subsquid lookups, best effort: any failure leaves the entry absent and is reported through
/// `errors()` so the UI log shows it.
pub struct Squid {
    client: reqwest::Client,
    endpoint: String,
    errors: std::sync::Mutex<Vec<String>>,
}

#[derive(Deserialize)]
struct GqlResponse<T> {
    data: Option<T>,
    errors: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct CommitmentsData {
    commitments: Vec<SquidCommitment>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SquidCommitment {
    hash: String,
    block_number: String,
    block_timestamp: Option<String>,
    transaction_hash: Option<String>,
}
#[derive(Deserialize)]
struct NullifiersData {
    nullifiers: Vec<SquidNullifier>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SquidNullifier {
    nullifier: String,
    block_number: String,
    block_timestamp: Option<String>,
    transaction_hash: Option<String>,
}
#[derive(Deserialize)]
struct UnshieldsData {
    unshields: Vec<SquidUnshield>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SquidUnshield {
    block_number: String,
    block_timestamp: Option<String>,
    transaction_hash: String,
    to: String,
    token: Option<SquidToken>,
    amount: String,
    fee: String,
    event_log_index: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SquidToken {
    token_address: Option<String>,
}
#[derive(Deserialize)]
struct TransactionsData {
    transactions: Vec<SquidTransaction>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SquidTransaction {
    transaction_hash: String,
    block_number: String,
    block_timestamp: Option<String>,
    #[serde(default)]
    nullifiers: Vec<String>,
    #[serde(default)]
    commitments: Vec<String>,
    #[serde(default)]
    has_unshield: Option<bool>,
    #[serde(default)]
    unshield_to_address: Option<String>,
    #[serde(default)]
    unshield_token: Option<SquidToken>,
    #[serde(default)]
    unshield_value: Option<String>,
}

fn parse_u64(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// Hex (`0x…`) or decimal string -> `0x` + 64 hex digits.
fn norm_any(s: &str) -> String {
    let t = s.trim();
    if t.starts_with("0x") || t.starts_with("0X") {
        return norm_hex(t);
    }
    match U256::from_str_radix(t, 10) {
        Ok(v) => format!("0x{v:064x}"),
        Err(_) => norm_hex(t),
    }
}

fn norm_hex(s: &str) -> String {
    let t = s.trim().trim_start_matches("0x").to_ascii_lowercase();
    format!("0x{:0>64}", t)
}

/// Subsquid stores hashes as decimal strings and nullifiers as hex.
fn to_decimal(hex_or_dec: &str) -> String {
    let t = hex_or_dec.trim();
    match U256::from_str_radix(t.trim_start_matches("0x"), 16) {
        Ok(v) if t.starts_with("0x") => v.to_string(),
        _ => t.to_string(),
    }
}

impl Squid {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
            errors: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn report(&self, what: &str, e: &anyhow::Error) {
        tracing::warn!("subsquid {what} lookup failed: {e:#}");
        if let Ok(mut v) = self.errors.lock() {
            v.push(format!("subsquid {what}: {e:#}"));
        }
    }

    /// Errors collected since the last call, oldest first.
    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
    }

    /// Operations mined in the given blocks (the `Transaction` entity). Two passes: the full
    /// field set first, then without the unshield fields if the indexer schema lacks them.
    pub async fn transactions_at(&self, blocks: &[u64]) -> Vec<OpRef> {
        const FULL: &str = "query Q($b: [BigInt!]) { transactions(where: {blockNumber_in: $b}, limit: 1000) \
            { transactionHash blockNumber blockTimestamp nullifiers commitments hasUnshield \
              unshieldToAddress unshieldToken { tokenAddress } unshieldValue } }";
        const MINIMAL: &str = "query Q($b: [BigInt!]) { transactions(where: {blockNumber_in: $b}, limit: 1000) \
            { transactionHash blockNumber blockTimestamp nullifiers commitments } }";
        let mut out = Vec::new();
        for chunk in blocks.chunks(100) {
            let vars = json!({ "b": chunk.iter().map(|b| b.to_string()).collect::<Vec<_>>() });
            let data = match self.query::<TransactionsData>(FULL, vars.clone()).await {
                Ok(d) => d,
                Err(e1) => match self.query::<TransactionsData>(MINIMAL, vars).await {
                    Ok(d) => {
                        tracing::debug!("subsquid transactions: unshield fields unavailable ({e1:#})");
                        d
                    }
                    Err(e2) => {
                        self.report("transactions", &e2);
                        continue;
                    }
                },
            };
            for t in data.transactions {
                out.push(OpRef {
                    transaction_hash: t.transaction_hash.to_ascii_lowercase(),
                    block_number: parse_u64(&t.block_number).unwrap_or_default(),
                    timestamp: t.block_timestamp.as_deref().and_then(parse_u64),
                    nullifiers: t.nullifiers.iter().map(|n| norm_any(n)).collect(),
                    commitments: t.commitments.iter().map(|c| norm_any(c)).collect(),
                    has_unshield: t.has_unshield.unwrap_or(false)
                        || t.unshield_value.as_deref().map(|v| v != "0").unwrap_or(false),
                    unshield_to: t.unshield_to_address.filter(|a| a.len() == 42),
                    unshield_token: t.unshield_token.and_then(|x| x.token_address),
                    unshield_value: t.unshield_value.filter(|v| v != "0"),
                });
            }
        }
        out
    }

    async fn query<T: serde::de::DeserializeOwned>(&self, query: &str, vars: Value) -> Result<T> {
        let body = serde_json::to_vec(&json!({ "query": query, "variables": vars }))?;
        let resp = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .context("subsquid request")?;
        let bytes = resp.bytes().await.context("subsquid body")?;
        let parsed: GqlResponse<T> = serde_json::from_slice(&bytes).context("subsquid json")?;
        if let Some(errs) = parsed.errors {
            if !errs.is_empty() {
                anyhow::bail!("subsquid: {}", serde_json::to_string(&errs).unwrap_or_default());
            }
        }
        parsed.data.context("subsquid: empty data")
    }

    /// Commitment hash (0x + 64 hex) -> chain reference.
    pub async fn commitments(&self, hashes: &[String]) -> HashMap<String, ChainRef> {
        let mut out = HashMap::new();
        for chunk in hashes.chunks(200) {
            let decimals: Vec<String> = chunk.iter().map(|h| to_decimal(h)).collect();
            let q = "query Q($hashes: [BigInt!]) { commitments(where: {hash_in: $hashes}, limit: 1000) \
                     { hash blockNumber blockTimestamp transactionHash } }";
            match self.query::<CommitmentsData>(q, json!({ "hashes": decimals })).await {
                Ok(data) => {
                    for c in data.commitments {
                        let key = match U256::from_str_radix(c.hash.trim_start_matches("0x"), 10) {
                            Ok(v) => format!("0x{v:064x}"),
                            Err(_) => norm_hex(&c.hash),
                        };
                        out.insert(
                            key,
                            ChainRef {
                                block_number: parse_u64(&c.block_number).unwrap_or_default(),
                                timestamp: c.block_timestamp.as_deref().and_then(parse_u64),
                                transaction_hash: c.transaction_hash,
                            },
                        );
                    }
                }
                Err(e) => self.report("commitments", &e),
            }
        }
        out
    }

    /// Nullifier (0x + 64 hex) -> chain reference, for the nullifiers spent in the given blocks.
    /// This squid has no `_in` filter on `Bytes` fields, only on numbers: query by block.
    pub async fn nullifiers_at(&self, blocks: &[u64]) -> HashMap<String, ChainRef> {
        let mut out = HashMap::new();
        for chunk in blocks.chunks(100) {
            let b: Vec<String> = chunk.iter().map(|x| x.to_string()).collect();
            let q = "query Q($b: [BigInt!]) { nullifiers(where: {blockNumber_in: $b}, limit: 1000) \
                     { nullifier blockNumber blockTimestamp transactionHash } }";
            match self.query::<NullifiersData>(q, json!({ "b": b })).await {
                Ok(data) => {
                    for n in data.nullifiers {
                        out.insert(
                            norm_hex(&n.nullifier),
                            ChainRef {
                                block_number: parse_u64(&n.block_number).unwrap_or_default(),
                                timestamp: n.block_timestamp.as_deref().and_then(parse_u64),
                                transaction_hash: n.transaction_hash,
                            },
                        );
                    }
                }
                Err(e) => self.report("nullifiers", &e),
            }
        }
        out
    }

    /// Unshield events mined in the given blocks (no `_in` on `Bytes` fields on this squid).
    pub async fn unshields_at(&self, blocks: &[u64]) -> Vec<UnshieldRef> {
        let mut out = Vec::new();
        for chunk in blocks.chunks(100) {
            let b: Vec<String> = chunk.iter().map(|x| x.to_string()).collect();
            let q = "query Q($b: [BigInt!]) { unshields(where: {blockNumber_in: $b}, limit: 1000) \
                     { blockNumber blockTimestamp transactionHash to token { tokenAddress } amount fee eventLogIndex } }";
            match self.query::<UnshieldsData>(q, json!({ "b": b })).await {
                Ok(data) => {
                    for u in data.unshields {
                        out.push(UnshieldRef {
                            transaction_hash: u.transaction_hash.to_ascii_lowercase(),
                            block_number: parse_u64(&u.block_number).unwrap_or_default(),
                            timestamp: u.block_timestamp.as_deref().and_then(parse_u64),
                            to: u.to,
                            token_address: u.token.and_then(|t| t.token_address),
                            amount: u.amount,
                            fee: u.fee,
                            event_log_index: u.event_log_index.as_deref().and_then(parse_u64),
                        });
                    }
                }
                Err(e) => self.report("unshields", &e),
            }
        }
        out
    }
}

//! Network health: RPC, subsquid, POI node and broadcasters, independent of any wallet.
//! Runs in the HTTP runtime (the broadcaster client is not tied to the engine thread).

use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

// `std::time::Instant::now` panics on wasm32-unknown-unknown.
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use alloy::{
    network::Ethereum,
    providers::{Provider, ProviderBuilder},
};
use railgun::chain_config::ChainConfig;
use railgun_broadcaster::{BroadcasterClient, NwakuRest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::shared::now_ms;

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    /// ok | warn | down | skip
    pub level: &'static str,
    pub latency_ms: Option<u64>,
    pub summary: String,
    pub details: Value,
    pub error: Option<String>,
}

impl Probe {
    fn down(summary: impl Into<String>, error: impl Into<String>) -> Self {
        Probe {
            level: "down",
            summary: summary.into(),
            error: Some(error.into()),
            ..Default::default()
        }
    }
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    pub checked_at: u64,
    pub chain_id: u64,
    pub rpc: Probe,
    pub subsquid: Probe,
    pub poi: Probe,
    pub broadcasters: Probe,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HealthParams {
    pub chain_id: u64,
    pub rpc_url: Option<String>,
    /// nwaku REST root, e.g. http://127.0.0.1:8645; when given it takes precedence
    pub waku_url: Option<String>,
    /// Waku node for the broadcaster probe: "native" (the viewer's own Rust light node, when the
    /// build has one), "tab" (the page's js-waku node) or "nwaku" (REST, with `waku_url`).
    /// Absent: nwaku if a URL is given, else native when available, else the tab.
    #[serde(default)]
    pub waku_source: Option<String>,
    /// seconds spent listening for broadcaster fee messages (default 15)
    pub listen_secs: Option<u64>,
}

/// Approximate block interval, to express an indexer lag in time rather than in blocks.
fn block_time_secs(chain_id: u64) -> f64 {
    match chain_id {
        1 | 11155111 => 12.0,
        56 => 1.5,
        137 => 2.0,
        42161 => 0.25,
        _ => 12.0,
    }
}

fn fmt_secs(s: f64) -> String {
    if s < 90.0 {
        format!("{s:.0} s")
    } else if s < 5400.0 {
        format!("{:.0} min", s / 60.0)
    } else {
        format!("{:.1} h", s / 3600.0)
    }
}

fn default_rpc_url(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("https://ethereum-rpc.publicnode.com"),
        11155111 => Some("https://ethereum-sepolia-rpc.publicnode.com"),
        56 => Some("https://bsc-rpc.publicnode.com"),
        137 => Some("https://polygon-bor-rpc.publicnode.com"),
        42161 => Some("https://arbitrum-one-rpc.publicnode.com"),
        _ => None,
    }
}

async fn gql(client: &reqwest::Client, endpoint: &str, query: &str) -> Result<Value, String> {
    let body = serde_json::to_vec(&json!({ "query": query })).map_err(|e| e.to_string())?;
    let resp = client
        .post(endpoint)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("http {status}: {e}"))?;
    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
        if !errs.is_empty() {
            return Err(errs
                .iter()
                .filter_map(|e| e.get("message").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; "));
        }
    }
    v.get("data").cloned().ok_or_else(|| "no data".into())
}

/// Why the default POI node can be out of reach: shown with network-level failures.
pub const POI_IPV6_HINT: &str = "ppoi.fdi.network publishes an IPv6 address that refuses connections: \
networks with IPv4 (Wi-Fi, most home and office networks) fall back to it, IPv6-only mobile networks \
cannot reach it. Use a network with IPv4.";

/// Whether this network reaches the POI node at all: `Err` only when the request did not get an
/// HTTP answer (connection refused, DNS, timeout, CORS), whatever the JSON-RPC result.
pub async fn poi_reachable(chain_id: u64, endpoint: &str) -> Result<(), String> {
    let params = json!({ "chainType": "0", "chainID": chain_id.to_string(), "txidVersion": "V2_PoseidonMerkle" });
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "ppoi_validated_txid", "params": params });
    http_client()
        .post(endpoint)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body).map_err(|e| e.to_string())?)
        .send()
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn rpc_call(client: &reqwest::Client, endpoint: &str, method: &str, params: Value) -> Result<Value, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let resp = client
        .post(endpoint)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body).map_err(|e| e.to_string())?)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("http {status}: {e}"))?;
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            return Err(err.get("message").and_then(Value::as_str).unwrap_or("rpc error").to_string());
        }
    }
    v.get("result").cloned().ok_or_else(|| "null result".into())
}

async fn sleep_ms(ms: u64) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms.min(u32::MAX as u64) as u32).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

fn http_client() -> reqwest::Client {
    #[cfg(not(target_arch = "wasm32"))]
    {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap_or_default()
    }
    // the fetch backend has no client-wide timeout; the browser's own applies
    #[cfg(target_arch = "wasm32")]
    {
        reqwest::Client::new()
    }
}

/// `bridge`: client over the page's js-waku node; `native`: client over the viewer's own Rust light
/// node (`LightNodeTransport`, daemon only). The chosen one is kept by the caller between runs.
pub async fn run(
    p: HealthParams,
    bridge: Option<Arc<BroadcasterClient>>,
    native: Option<Arc<BroadcasterClient>>,
) -> HealthReport {
    let chain = ChainConfig::from_chain_id(p.chain_id);
    let client = http_client();

    // ---- RPC ----
    let rpc_url = p
        .rpc_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .or_else(|| default_rpc_url(p.chain_id).map(str::to_string));
    let mut head: Option<u64> = None;
    let rpc = match rpc_url.as_deref() {
        None => Probe::down("no RPC", "no RPC URL for this chain"),
        Some(url) => {
            let t0 = Instant::now();
            match ProviderBuilder::new().network::<Ethereum>().connect(url).await {
                Err(e) => Probe::down("unreachable", e.to_string()),
                Ok(provider) => {
                    let provider = provider.erased();
                    let cid = provider.get_chain_id().await;
                    let bn = provider.get_block_number().await;
                    let latency = t0.elapsed().as_millis() as u64;
                    match (cid, bn) {
                        (Ok(cid), Ok(bn)) => {
                            head = Some(bn);
                            let level = if cid == p.chain_id { "ok" } else { "down" };
                            Probe {
                                level,
                                latency_ms: Some(latency),
                                summary: format!("chain {cid} · head #{bn}"),
                                details: json!({ "url": url, "chainId": cid, "head": bn }),
                                error: (cid != p.chain_id).then(|| format!("RPC is on chain {cid}, expected {}", p.chain_id)),
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => Probe::down("error", e.to_string()),
                    }
                }
            }
        }
    };

    // ---- subsquid ----
    let subsquid = match &chain {
        None => Probe::down("unknown chain", format!("no chain config for {}", p.chain_id)),
        Some(c) => {
            let t0 = Instant::now();
            // `squidStatus.height` is the finalized height on this squid (entities exist beyond
            // it); the indexed head is the newest block of the data itself.
            let status = match gql(&client, &c.subsquid_endpoint, "{ squidStatus { height finalizedHeight } }").await {
                Ok(d) => Ok(d),
                Err(_) => gql(&client, &c.subsquid_endpoint, "{ squidStatus { height } }").await,
            };
            let latency = t0.elapsed().as_millis() as u64;
            match status {
                Err(e) => Probe::down("unreachable", e),
                Ok(d) => {
                    let num = |v: Option<&Value>| v.and_then(|h| h.as_u64().or_else(|| h.as_str().and_then(|s| s.parse().ok())));
                    let height = num(d.pointer("/squidStatus/height")).unwrap_or(0);
                    let finalized = num(d.pointer("/squidStatus/finalizedHeight"));
                    // the entity the txid tree and the viewer depend on, and its unshield fields
                    let full = gql(&client, &c.subsquid_endpoint,
                        "{ transactions(limit: 1, orderBy: blockNumber_DESC) { transactionHash blockNumber unshieldValue } \
                           commitments(limit: 1, orderBy: blockNumber_DESC) { blockNumber } \
                           transactionsConnection(orderBy: id_ASC) { totalCount } }").await;
                    let (tx_ok, unshield_fields, total_ops, last_op_block, last_commit_block) = match full {
                        Ok(v) => (
                            true,
                            true,
                            v.pointer("/transactionsConnection/totalCount").and_then(Value::as_u64),
                            num(v.pointer("/transactions/0/blockNumber")),
                            num(v.pointer("/commitments/0/blockNumber")),
                        ),
                        Err(_) => {
                            let min = gql(&client, &c.subsquid_endpoint,
                                "{ transactions(limit: 1, orderBy: blockNumber_DESC) { transactionHash blockNumber } \
                                   commitments(limit: 1, orderBy: blockNumber_DESC) { blockNumber } \
                                   transactionsConnection(orderBy: id_ASC) { totalCount } }").await;
                            match min {
                                Ok(v) => (
                                    true,
                                    false,
                                    v.pointer("/transactionsConnection/totalCount").and_then(Value::as_u64),
                                    num(v.pointer("/transactions/0/blockNumber")),
                                    num(v.pointer("/commitments/0/blockNumber")),
                                ),
                                Err(_) => (false, false, None, None, None),
                            }
                        }
                    };
                    let indexed_head = [Some(height), last_op_block, last_commit_block]
                        .into_iter()
                        .flatten()
                        .max()
                        .unwrap_or(height);
                    let lag = head.map(|h| h.saturating_sub(indexed_head));
                    // an indexer with no Railgun activity in a while cannot be told apart from a
                    // stalled one by data alone: the finalized height gives the second signal
                    let finalized_lag = head.zip(finalized).map(|(h, f)| h.saturating_sub(f));
                    // thresholds in time: 5 min ok, 30 min warn, beyond that down
                    let bt = block_time_secs(p.chain_id);
                    let lag_secs = lag.map(|l| l as f64 * bt);
                    let fin_secs = finalized_lag.map(|f| f as f64 * bt);
                    let level = match (tx_ok, lag_secs, fin_secs) {
                        (false, _, _) => "warn",
                        (true, Some(l), Some(f)) if l > 1800.0 && f > 1800.0 => "down",
                        (true, Some(l), None) if l > 1800.0 => "down",
                        (true, Some(l), _) if l > 300.0 => "warn",
                        _ => "ok",
                    };
                    Probe {
                        level,
                        latency_ms: Some(latency),
                        summary: match (lag, lag_secs) {
                            (Some(l), Some(s)) => format!("indexed up to #{indexed_head} · {l} block(s) ≈ {} behind the RPC head", fmt_secs(s)),
                            _ => format!("indexed up to #{indexed_head}"),
                        },
                        details: json!({
                            "endpoint": c.subsquid_endpoint, "squidStatusHeight": height, "finalizedHeight": finalized,
                            "indexedHead": indexed_head, "lag": lag, "lagSeconds": lag_secs, "blockTimeSecs": bt, "finalizedLag": finalized_lag,
                            "transactionsEntity": tx_ok, "unshieldFields": unshield_fields,
                            "totalOperations": total_ops, "lastOperationBlock": last_op_block, "lastCommitmentBlock": last_commit_block,
                        }),
                        error: (!tx_ok).then(|| "transactions entity not queryable".to_string()),
                    }
                }
            }
        }
    };
    let total_ops = subsquid.details.get("totalOperations").and_then(Value::as_u64);

    // ---- POI node ----
    let poi = match &chain {
        None => Probe::down("unknown chain", "no chain config"),
        Some(c) => {
            let chain_params = json!({ "chainType": "0", "chainID": p.chain_id.to_string(), "txidVersion": "V2_PoseidonMerkle" });
            let t0 = Instant::now();
            let validated = rpc_call(&client, &c.poi_endpoint, "ppoi_validated_txid", chain_params.clone()).await;
            let latency = t0.elapsed().as_millis() as u64;
            match validated {
                // a JSON-RPC error is an answer; no answer at all means the network does not reach
                // the node, which the default node's dead IPv6 explains on IPv6-only networks
                Err(e) if poi_reachable(p.chain_id, &c.poi_endpoint).await.is_err() => {
                    let hint = if c.poi_endpoint.contains("ppoi.fdi.network") { format!(" · {POI_IPV6_HINT}") } else { String::new() };
                    Probe::down(&format!("unreachable from this network{hint}"), e)
                }
                Err(e) => Probe::down("unreachable", e),
                Ok(v) => {
                    let idx = v.get("validatedTxidIndex").and_then(Value::as_u64);
                    let root = v
                        .get("validatedMerkleroot")
                        .or_else(|| v.get("validatedTxidMerkleroot"))
                        .or_else(|| v.get("merkleroot"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    // operations the node has not validated yet (subsquid total as the chain-side count)
                    let behind = match (total_ops, idx) {
                        (Some(t), Some(i)) => Some(t.saturating_sub(i + 1)),
                        _ => None,
                    };
                    let (status, status_error) = match rpc_call(&client, &c.poi_endpoint, "ppoi_node_status_v2", json!({})).await {
                        Ok(s) => (Some(s), None),
                        Err(e1) => match rpc_call(&client, &c.poi_endpoint, "ppoi_node_status_v2", chain_params.clone()).await {
                            Ok(s) => (Some(s), None),
                            Err(e2) => (None, Some(format!("{e1} / {e2}"))),
                        },
                    };
                    let list_keys: Vec<String> = c
                        .list_keys
                        .iter()
                        .filter_map(|k| serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_owned)))
                        .collect();
                    let served: Vec<String> = status
                        .as_ref()
                        .and_then(|s| s.get("listKeys").and_then(Value::as_array))
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                        .unwrap_or_default();
                    let missing_lists: Vec<&String> = list_keys.iter().filter(|k| !served.is_empty() && !served.contains(k)).collect();
                    let level = if !missing_lists.is_empty() {
                        "down"
                    } else {
                        match behind {
                            Some(b) if b > 50 => "warn",
                            _ => "ok",
                        }
                    };
                    Probe {
                        level,
                        latency_ms: Some(latency),
                        summary: match (idx, behind) {
                            (Some(i), Some(b)) => format!("validated txid index {i} · {b} operation(s) not yet validated"),
                            (Some(i), None) => format!("validated txid index {i}"),
                            _ => "reachable".into(),
                        },
                        details: json!({
                            "endpoint": c.poi_endpoint, "validatedTxidIndex": idx, "validatedMerkleroot": root,
                            "operationsBehind": behind, "configuredLists": list_keys, "servedLists": served,
                            "validatedRaw": v, "nodeStatus": status, "nodeStatusError": status_error,
                        }),
                        error: (!missing_lists.is_empty()).then(|| format!("list(s) not served by the node: {:?}", missing_lists)),
                    }
                }
            }
        }
    };

    // ---- broadcasters (Waku fee messages): nwaku REST when a URL is given, else the viewer's
    // Rust node or the tab's js-waku node ----
    let listen = p.listen_secs.unwrap_or(15).clamp(2, 90);
    let fee_token = chain
        .as_ref()
        .map(|c| format!("{:?}", c.wrapped_base_token).to_ascii_lowercase())
        .unwrap_or_default();
    let nwaku_url = p.waku_url.as_deref().map(str::trim).filter(|u| !u.is_empty()).map(str::to_string);
    let tab = |b: &Option<Arc<BroadcasterClient>>| match b {
        Some(b) => (Some(b.clone()), "tab"),
        None => (None, "none"),
    };
    let (bc, source): (Option<Arc<BroadcasterClient>>, &str) = match (&nwaku_url, p.waku_source.as_deref()) {
        (Some(url), _) => (Some(Arc::new(BroadcasterClient::new(Arc::new(NwakuRest::new(url.clone())), p.chain_id))), "nwaku"),
        (None, Some("tab")) => tab(&bridge),
        (None, _) => match &native {
            Some(n) => (Some(n.clone()), "native"),
            None => tab(&bridge),
        },
    };
    let broadcasters = match bc {
        None => Probe {
            level: "skip",
            summary: "no Waku node: choose one in the Network tab or give an nwaku URL".into(),
            ..Default::default()
        },
        Some(bc) => {
            let url = nwaku_url.clone().unwrap_or_else(|| source.to_string());
            let t0 = Instant::now();
            let mut subscribed = bc.subscribe().await;
            if source == "native" {
                // the first call starts the node, which answers "not ready" until it has dialled
                // the fleet and opened its filter subscriptions (a few seconds): retry for a minute
                let deadline = Instant::now() + std::time::Duration::from_secs(60);
                while subscribed.is_err() && Instant::now() < deadline {
                    sleep_ms(1000).await;
                    subscribed = bc.subscribe().await;
                }
            }
            match subscribed {
                Err(e) => {
                    let msg = e.to_string();
                    let local = msg.contains("error sending request") || msg.contains("unreachable") || msg.contains("connection refused");
                    Probe {
                        level: if local { "skip" } else { "down" },
                        latency_ms: Some(t0.elapsed().as_millis() as u64),
                        summary: if local {
                            format!("no nwaku node reachable at {url}: broadcaster probe skipped")
                        } else if source == "native" {
                            "the viewer's Waku node could not subscribe within a minute".into()
                        } else {
                            "subscribe failed".into()
                        },
                        details: json!({ "wakuUrl": url }),
                        error: Some(msg),
                    }
                }
                Ok(()) => {
                    // the tab's node needs up to a minute after opening the tab, the viewer's node a
                    // few seconds on its first run: wait for peers before the listen window instead
                    // of reporting an empty network
                    let mut waited = 0u64;
                    if source == "tab" || source == "native" {
                        while waited < 60 {
                            if matches!(bc.peer_count().await, Some(n) if n > 0) {
                                break;
                            }
                            sleep_ms(1000).await;
                            waited += 1;
                        }
                    }
                    let deadline = Instant::now() + std::time::Duration::from_secs(listen);
                    let mut received = 0usize;
                    let mut pump_err: Option<String> = None;
                    while Instant::now() < deadline {
                        match bc.pump().await {
                            Ok(n) => received += n,
                            Err(e) => {
                                pump_err = Some(e.to_string());
                                // the Rust node reconnects by itself when a fleet node drops it:
                                // keep listening instead of ending the window on the first gap
                                if source != "native" {
                                    break;
                                }
                            }
                        }
                        sleep_ms(500).await;
                    }
                    let peers = bc.peer_count().await;
                    let now = now_ms();
                    let quotes = bc.all_quotes();
                    let mut signers: Vec<String> = quotes.iter().map(|q| q.railgun_address.clone()).collect();
                    signers.sort();
                    signers.dedup();
                    let usable_base: usize = quotes
                        .iter()
                        .filter(|q| q.token.to_ascii_lowercase() == fee_token && q.usable_at(now))
                        .count();
                    let list: Vec<Value> = quotes
                        .iter()
                        .map(|q| json!({
                            "broadcaster": q.railgun_address, "identifier": q.identifier, "token": q.token,
                            "feePerUnitGas": q.fee_per_unit_gas.to_string(), "expiresInSec": (q.expiration as i128 - now as i128) / 1000,
                            "availableWallets": q.available_wallets, "reliability": q.reliability,
                            "requiredPoiListKeys": q.required_poi_list_keys, "usable": q.usable_at(now), "version": q.version,
                        }))
                        .collect();
                    let tab_offline = source == "tab" && peers.is_none() && received == 0 && signers.is_empty();
                    let native_offline =
                        source == "native" && peers.unwrap_or(0) == 0 && received == 0 && signers.is_empty();
                    let level = if tab_offline {
                        "skip"
                    } else if native_offline {
                        "down"
                    } else if pump_err.is_some() && signers.is_empty() {
                        "down"
                    } else if signers.is_empty() || usable_base == 0 {
                        "warn"
                    } else {
                        "ok"
                    };
                    Probe {
                        level,
                        latency_ms: Some(t0.elapsed().as_millis() as u64),
                        summary: if tab_offline {
                            "the tab's Waku node found no peer within a minute (see the status line above)".into()
                        } else if native_offline {
                            format!(
                                "the viewer's Waku node found no peer within a minute{}",
                                pump_err.as_ref().map(|e| format!(": {e}")).unwrap_or_default()
                            )
                        } else {
                            format!(
                                "{} broadcaster(s) · {} usable quote(s) for the base token · {} message(s) in {listen}s · peers {} · via {source}",
                                signers.len(),
                                usable_base,
                                received,
                                peers.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
                            )
                        },
                        details: json!({ "source": source, "wakuUrl": url, "peers": peers, "messages": received, "listenSecs": listen, "waitedForPeersSecs": waited, "feeToken": fee_token, "signers": signers, "quotes": list }),
                        error: pump_err,
                    }
                }
            }
        }
    };

    HealthReport {
        checked_at: now_ms(),
        chain_id: p.chain_id,
        rpc,
        subsquid,
        poi,
        broadcasters,
    }
}

//! Wire format of the broadcaster protocol, as defined by `@railgun-community/shared-models`
//! and `waku-broadcaster-client` 9.x.

use std::collections::BTreeMap;

use railgun::poi::PreTransactionPois;
use serde::{Deserialize, Serialize};

use crate::crypto::EncryptedData;

/// Range of broadcaster versions this client speaks to (`BroadcasterConfig` in the reference
/// client).
pub const MIN_BROADCASTER_VERSION: &str = "8.0.0";
pub const MAX_BROADCASTER_VERSION: &str = "8.999.0";

/// EVM chains are type 0 in Railgun's `Chain`.
pub const CHAIN_TYPE_EVM: u8 = 0;

pub const TXID_VERSION_V2: &str = "V2_PoseidonMerkle";

/// Relay shard the Railgun fleet publishes on (cluster 5, shard 1).
pub const PUBSUB_TOPIC: &str = "/waku/2/rs/5/1";

pub const CLUSTER_ID: u32 = 5;
pub const SHARD_ID: u32 = 1;

/// Secure WebSocket peers of the Railgun fleet, for light nodes running in a browser
/// (`WAKU_RAILGUN_DEFAULT_PEERS_WEB` of the reference client 9.1.1).
pub const FLEET_WSS_PEERS: [&str; 3] = [
    "/dns4/relay-a.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmFbD2ZvAFi2j9jjDo6g4HFbQAhfjDfnTTrbyRGQRmtG7x",
    "/dns4/relay-b.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmPtEAoPPok7VLrpNNC6t92ZQFqLndHvkdx6Fk3CxA4MaG",
    "/dns4/client-edge.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmQdCGG5qREQCq96kucmpUVupmvLwrTRjMazPAaMTNP97A",
];

pub fn fees_topic(chain_id: u64) -> String {
    format!("/railgun/v2/{CHAIN_TYPE_EVM}-{chain_id}-fees/json")
}

pub fn transact_topic(chain_id: u64) -> String {
    format!("/railgun/v2/{CHAIN_TYPE_EVM}-{chain_id}-transact/json")
}

pub fn transact_response_topic(chain_id: u64) -> String {
    format!("/railgun/v2/{CHAIN_TYPE_EVM}-{chain_id}-transact-response/json")
}

/// Payload of a fees message: `data` is the hex of the UTF-8 JSON of [`FeeMessageData`],
/// `signature` an ed25519 signature of those bytes by the broadcaster's viewing key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeMessage {
    pub data: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeMessageData {
    /// Token address to fee per unit of gas (hex), see [`crate::fees::token_fee`].
    pub fees: BTreeMap<String, String>,
    /// Milliseconds since the epoch.
    pub fee_expiration: u64,
    #[serde(rename = "feesID")]
    pub fees_id: String,
    pub railgun_address: String,
    #[serde(default)]
    pub identifier: Option<String>,
    pub available_wallets: u32,
    pub version: String,
    #[serde(default)]
    pub relay_adapt: String,
    #[serde(rename = "requiredPOIListKeys", default)]
    pub required_poi_list_keys: Vec<String>,
    #[serde(default)]
    pub reliability: f64,
}

/// Cleartext of a transact request (`BroadcasterRawParamsTransactCommon`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactParams {
    pub transact_type: String,
    pub txid_version: String,
    /// Checksummed contract address.
    pub to: String,
    /// `0x` calldata.
    pub data: String,
    /// Broadcaster viewing public key, hex without prefix.
    pub broadcaster_viewing_key: String,
    #[serde(rename = "chainID")]
    pub chain_id: u64,
    pub chain_type: u8,
    /// Decimal string, wei.
    pub min_gas_price: String,
    #[serde(rename = "feesID")]
    pub fees_id: String,
    pub use_relay_adapt: bool,
    pub dev_log: bool,
    pub min_version: String,
    pub max_version: String,
    #[serde(rename = "preTransactionPOIsPerTxidLeafPerList")]
    pub pre_transaction_pois: PreTransactionPois,
}

/// What actually goes on the transact topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactMessage {
    pub method: String,
    pub params: EncryptedParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptedParams {
    /// Ephemeral ed25519 public key of the wallet, hex without prefix.
    pub pubkey: String,
    pub encrypted_data: EncryptedData,
}

/// Payload on the transact-response topic. `result` is sealed with the request's shared key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactResponseMessage {
    pub result: EncryptedData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub tx_hash: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

//! A broadcaster that lives in the test process: announces fees, opens transact requests,
//! answers with a transaction hash. It does not touch a chain.

use std::{collections::BTreeMap, sync::Arc};

use ed25519_dalek::{Signer, SigningKey};
use railgun::{
    account::{
        chain::ChainId,
        signer::{PrivateKeySigner, RailgunSigner},
    },
    crypto::keys::{HexKey, SpendingKey, ViewingKey},
};

use crate::{
    crypto,
    fees::now_ms,
    transport::WakuTransport,
    wire::{
        self, FeeMessage, FeeMessageData, TransactMessage, TransactParams, TransactResponse,
        TransactResponseMessage,
    },
};

pub struct MockBroadcaster {
    pub railgun_address: String,
    /// Same keys, chain-agnostic form of the address (what wallets usually display).
    pub railgun_address_all_chains: String,
    /// `requiredPOIListKeys` of the announcements. Real broadcasters set the active list.
    pub required_poi_list_keys: Vec<String>,
    viewing_secret: [u8; 32],
    transport: Arc<dyn WakuTransport>,
    chain_id: u64,
}

pub enum MockAnswer {
    TxHash(String),
    Error(String),
}

impl MockBroadcaster {
    pub fn new(transport: Arc<dyn WakuTransport>, chain_id: u64, seed: u8) -> Self {
        let viewing_secret = [seed; 32];
        let spending = || SpendingKey::from_hex(&hex::encode([seed.wrapping_add(1); 32])).unwrap();
        let viewing = || ViewingKey::from_hex(&hex::encode(viewing_secret)).unwrap();
        let signer = PrivateKeySigner::new_evm(spending(), viewing(), chain_id);
        let unscoped = PrivateKeySigner::new(spending(), viewing(), ChainId::All);
        Self {
            railgun_address: signer.address().to_string(),
            railgun_address_all_chains: unscoped.address().to_string(),
            required_poi_list_keys: Vec::new(),
            viewing_secret,
            transport,
            chain_id,
        }
    }

    pub async fn announce(&self, fees: &[(&str, u128)], fees_id: &str, ttl_ms: u64) {
        self.announce_as(fees, fees_id, ttl_ms, "8.2.0", &self.viewing_secret)
            .await;
    }

    pub async fn announce_as(
        &self,
        fees: &[(&str, u128)],
        fees_id: &str,
        ttl_ms: u64,
        version: &str,
        signing_secret: &[u8; 32],
    ) {
        let data = FeeMessageData {
            fees: fees
                .iter()
                .map(|(t, f)| (t.to_string(), format!("0x{f:x}")))
                .collect::<BTreeMap<_, _>>(),
            fee_expiration: now_ms() + ttl_ms,
            fees_id: fees_id.into(),
            railgun_address: self.railgun_address.clone(),
            identifier: Some("mock".into()),
            available_wallets: 1,
            version: version.into(),
            relay_adapt: String::new(),
            required_poi_list_keys: self.required_poi_list_keys.clone(),
            reliability: 0.99,
        };
        let json = serde_json::to_vec(&data).unwrap();
        let signature = SigningKey::from_bytes(signing_secret).sign(&json);
        let message = FeeMessage {
            data: hex::encode(&json),
            signature: hex::encode(signature.to_bytes()),
        };
        self.transport
            .publish(
                &wire::fees_topic(self.chain_id),
                &serde_json::to_vec(&message).unwrap(),
            )
            .await
            .unwrap();
    }

    /// Opens every transact request addressed to this broadcaster and answers it.
    pub async fn serve(&self, answer: impl Fn(&TransactParams) -> MockAnswer) -> Vec<TransactParams> {
        let mut served = Vec::new();
        for message in self.transport.poll().await.unwrap() {
            if message.content_topic != wire::transact_topic(self.chain_id) {
                continue;
            }
            let Ok(request) = serde_json::from_slice::<TransactMessage>(&message.payload) else {
                continue;
            };
            let Ok(pubkey) = hex::decode(&request.params.pubkey) else {
                continue;
            };
            let Ok(pubkey) = <[u8; 32]>::try_from(pubkey) else {
                continue;
            };
            let Ok(shared) = crypto::shared_secret(&self.viewing_secret, &pubkey) else {
                continue;
            };
            let Ok(params) =
                crypto::open_json::<TransactParams>(&request.params.encrypted_data, &shared)
            else {
                continue; // for another broadcaster
            };

            let response = match answer(&params) {
                MockAnswer::TxHash(hash) => TransactResponse {
                    id: Some("1".into()),
                    tx_hash: Some(hash),
                    error: None,
                },
                MockAnswer::Error(error) => TransactResponse {
                    id: Some("1".into()),
                    tx_hash: None,
                    error: Some(error),
                },
            };
            let sealed = crypto::seal_json(&response, &shared, &mut rand::rng()).unwrap();
            self.transport
                .publish(
                    &wire::transact_response_topic(self.chain_id),
                    &serde_json::to_vec(&TransactResponseMessage { result: sealed }).unwrap(),
                )
                .await
                .unwrap();
            served.push(params);
        }
        served
    }
}

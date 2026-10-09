//! Broadcaster client: keeps the fee cache current and relays one transaction at a time.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::CryptoRng;
use railgun::poi::PreTransactionPois;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::{
    crypto::{self, CryptoError, EphemeralKey},
    fees::{self, FeeCache, FeeError, FeeQuote, NoQuote, TrustPolicy},
    transport::{TransportError, WakuTransport},
    wire::{
        self, CHAIN_TYPE_EVM, EncryptedParams, MAX_BROADCASTER_VERSION, MIN_BROADCASTER_VERSION,
        TXID_VERSION_V2, TransactMessage, TransactParams, TransactResponse,
        TransactResponseMessage,
    },
};

/// The request is published again every `RETRY_INTERVAL` during `REPUBLISH_WINDOW`, then the
/// client only listens until `RESPONSE_TIMEOUT`. Same schedule as the reference client.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const REPUBLISH_WINDOW: Duration = Duration::from_secs(20);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Responses are broadcast to everyone and only the addressee can open them: keep a short
/// backlog so one that arrives between two polls of `send` is not lost to the fee monitor.
const RESPONSE_BACKLOG: usize = 256;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Fee(#[from] FeeError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("broadcaster refused the transaction: {0}")]
    Refused(String),
    #[error(
        "no answer from the broadcaster after {0:?}. The transaction may still have been sent: \
         check the nullifiers on-chain before retrying"
    )]
    Timeout(Duration),
}

/// Everything the broadcaster needs, before encryption.
pub struct BroadcastRequest {
    pub quote: FeeQuote,
    /// Checksummed address of the contract to call (Railgun smart wallet, or RelayAdapt).
    pub to: String,
    pub calldata: Vec<u8>,
    /// Must equal the `minGasPrice` bound in the proofs.
    pub min_gas_price: u128,
    pub use_relay_adapt: bool,
    pub pre_transaction_pois: PreTransactionPois,
}

/// A request sealed for one broadcaster. Holds the key that opens the response.
pub struct SealedRequest {
    payload: Vec<u8>,
    shared_key: [u8; 32],
    broadcaster: String,
}

pub struct BroadcasterClient {
    transport: Arc<dyn WakuTransport>,
    chain_id: u64,
    cache: Mutex<FeeCache>,
    responses: Mutex<VecDeque<TransactResponseMessage>>,
}

impl BroadcasterClient {
    pub fn new(transport: Arc<dyn WakuTransport>, chain_id: u64) -> Self {
        Self {
            transport,
            chain_id,
            cache: Mutex::new(FeeCache::default()),
            responses: Mutex::new(VecDeque::new()),
        }
    }

    /// Same as [`Self::new`], with offers restricted to a band around the rates announced by
    /// trusted fee signers (0zk addresses). See [`TrustPolicy`].
    pub fn with_trusted_signers(
        transport: Arc<dyn WakuTransport>,
        chain_id: u64,
        signer_addresses: &[String],
    ) -> Result<Self, ClientError> {
        Ok(Self {
            transport,
            chain_id,
            cache: Mutex::new(FeeCache::with_policy(TrustPolicy::new(signer_addresses)?)),
            responses: Mutex::new(VecDeque::new()),
        })
    }

    /// Number of trusted fee signers configured, 0 when every authenticated offer is accepted.
    pub fn trusted_signer_count(&self) -> usize {
        self.cache
            .lock()
            .unwrap()
            .policy()
            .map_or(0, TrustPolicy::signer_count)
    }

    /// Authorized rate per token, from the trusted signers' live announcements.
    pub fn authorized_fees(&self) -> Vec<(String, u128)> {
        self.cache.lock().unwrap().authorized_fees(fees::now_ms())
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub async fn subscribe(&self) -> Result<(), ClientError> {
        Ok(self.transport.subscribe().await?)
    }

    pub async fn peer_count(&self) -> Option<usize> {
        self.transport.peer_count().await
    }

    /// Publish counters of the transport, when it keeps them (see
    /// [`WakuTransport::publish_stats`]).
    pub fn publish_stats(&self) -> Option<crate::PublishStats> {
        self.transport.publish_stats()
    }

    /// Drains the transport once and files what arrived. Call it periodically.
    /// Returns the number of fee announcements accepted.
    pub async fn pump(&self) -> Result<usize, ClientError> {
        let messages = self.transport.poll().await?;
        let fees_topic = wire::fees_topic(self.chain_id);
        let response_topic = wire::transact_response_topic(self.chain_id);
        let now = fees::now_ms();

        let mut accepted = 0;
        for message in messages {
            if message.content_topic == fees_topic {
                match fees::parse_fee_message(&message.payload, now) {
                    Ok(data) => {
                        self.cache.lock().unwrap().insert(data);
                        accepted += 1;
                    }
                    Err(e) => debug!("fee message dropped: {e}"),
                }
            } else if message.content_topic == response_topic {
                if let Ok(response) = serde_json::from_slice(&message.payload) {
                    let mut backlog = self.responses.lock().unwrap();
                    backlog.push_back(response);
                    while backlog.len() > RESPONSE_BACKLOG {
                        backlog.pop_front();
                    }
                }
            }
        }
        self.cache.lock().unwrap().prune(now);
        Ok(accepted)
    }

    /// Usable quotes for a fee token, best first.
    pub fn quotes_for(&self, token: &str, our_list_keys: &[String]) -> Vec<FeeQuote> {
        self.cache
            .lock()
            .unwrap()
            .quotes_for(token, our_list_keys, fees::now_ms())
    }

    /// Cheapest usable offer under an optional rate ceiling. For the wrapped base token, express
    /// the ceiling as a multiple of [`fees::PAR_RATE_WRAPPED_BASE_TOKEN`].
    pub fn best_quote(
        &self,
        token: &str,
        our_list_keys: &[String],
        max_rate: Option<u128>,
    ) -> Result<FeeQuote, NoQuote> {
        self.cache
            .lock()
            .unwrap()
            .best_quote(token, our_list_keys, max_rate, fees::now_ms())
    }

    /// See [`FeeCache::select_quote`]. `pick(n)` returns an index below `n`.
    pub fn select_quote(
        &self,
        token: &str,
        our_list_keys: &[String],
        max_rate: Option<u128>,
        within_percent: u32,
        exclude: &[String],
        pick: impl FnOnce(usize) -> usize,
    ) -> Result<FeeQuote, NoQuote> {
        self.cache.lock().unwrap().select_quote(
            token,
            our_list_keys,
            max_rate,
            within_percent,
            exclude,
            pick,
            fees::now_ms(),
        )
    }

    pub fn all_quotes(&self) -> Vec<FeeQuote> {
        self.cache.lock().unwrap().all(fees::now_ms())
    }

    /// Encrypts a request for its broadcaster with a fresh ephemeral key.
    pub fn seal(
        &self,
        request: BroadcastRequest,
        rng: &mut impl CryptoRng,
    ) -> Result<SealedRequest, ClientError> {
        let broadcaster_key = request.quote.viewing_public_key()?;
        let params = TransactParams {
            transact_type: "COMMON".into(),
            txid_version: TXID_VERSION_V2.into(),
            to: request.to,
            data: format!("0x{}", hex::encode(&request.calldata)),
            broadcaster_viewing_key: hex::encode(broadcaster_key),
            chain_id: self.chain_id,
            chain_type: CHAIN_TYPE_EVM,
            min_gas_price: request.min_gas_price.to_string(),
            fees_id: request.quote.fees_id.clone(),
            use_relay_adapt: request.use_relay_adapt,
            dev_log: false,
            min_version: MIN_BROADCASTER_VERSION.into(),
            max_version: MAX_BROADCASTER_VERSION.into(),
            pre_transaction_pois: request.pre_transaction_pois,
        };

        let ephemeral = EphemeralKey::generate(rng);
        let shared_key = ephemeral.shared_secret(&broadcaster_key)?;
        let message = TransactMessage {
            method: "transact".into(),
            params: EncryptedParams {
                pubkey: ephemeral.public_hex(),
                encrypted_data: crypto::seal_json(&params, &shared_key, rng)?,
            },
        };
        Ok(SealedRequest {
            payload: serde_json::to_vec(&message)?,
            shared_key,
            broadcaster: request.quote.railgun_address,
        })
    }

    /// Publishes the request and waits for the broadcaster's answer: the transaction hash.
    pub async fn send(&self, request: &SealedRequest) -> Result<String, ClientError> {
        self.send_with_timeout(request, RESPONSE_TIMEOUT).await
    }

    pub async fn send_with_timeout(
        &self,
        request: &SealedRequest,
        timeout: Duration,
    ) -> Result<String, ClientError> {
        let topic = wire::transact_topic(self.chain_id);
        // Answers to earlier requests cannot be ours.
        self.responses.lock().unwrap().clear();

        let started = crate::time::Instant::now();
        let mut last_publish: Option<crate::time::Instant> = None;
        info!(
            "relaying through broadcaster {}…",
            &request.broadcaster[..request.broadcaster.len().min(16)]
        );

        loop {
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(ClientError::Timeout(timeout));
            }
            let due = last_publish.is_none_or(|t| t.elapsed() >= RETRY_INTERVAL);
            if due && elapsed <= REPUBLISH_WINDOW {
                match self.transport.publish(&topic, &request.payload).await {
                    Ok(()) => debug!("transact request published"),
                    Err(e) => warn!("publish failed, will retry: {e}"),
                }
                last_publish = Some(crate::time::Instant::now());
            }

            if let Err(e) = self.pump().await {
                warn!("waku poll failed: {e}");
            }
            if let Some(response) = self.take_response(&request.shared_key) {
                if let Some(hash) = response.tx_hash {
                    return Ok(hash);
                }
                return Err(ClientError::Refused(
                    response.error.unwrap_or_else(|| "no reason given".into()),
                ));
            }
            crate::time::sleep(POLL_INTERVAL).await;
        }
    }

    fn take_response(&self, key: &[u8; 32]) -> Option<TransactResponse> {
        let mut backlog = self.responses.lock().unwrap();
        let found = backlog
            .iter()
            .find_map(|m| crypto::open_json::<TransactResponse>(&m.result, key).ok());
        if found.is_some() {
            backlog.clear();
        }
        found
    }
}

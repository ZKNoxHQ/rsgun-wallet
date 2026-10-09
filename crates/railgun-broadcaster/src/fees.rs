//! Fee announcements: validation, cache, broadcaster selection and fee arithmetic.

use std::{
    collections::HashMap,
    str::FromStr,
};

use railgun::{account::address::RailgunAddress, crypto::keys::HexKey};
use serde::Serialize;
use thiserror::Error;
use tracing::debug;

use crate::{
    crypto::{self, strip0x},
    wire::{FeeMessage, FeeMessageData, MAX_BROADCASTER_VERSION, MIN_BROADCASTER_VERSION},
};

/// A quote is unusable when it expires sooner than this: the wallet still has to prove the
/// transaction and its POIs. After expiry the broadcaster answers "Bad token fee".
pub const MIN_TIME_TO_EXPIRY_MS: u64 = 40_000;

/// Announcements older than this are dropped on arrival.
const MAX_ANNOUNCEMENT_AGE_MS: u64 = 45_000;

/// Gas limit is the estimate plus 20%, as in `calculateGasLimit`.
const GAS_LIMIT_NUM: u128 = 12_000;
const GAS_LIMIT_DEN: u128 = 10_000;

#[derive(Debug, Error)]
pub enum FeeError {
    #[error("malformed fee message: {0}")]
    Malformed(String),
    #[error("stale fee message")]
    Stale,
    #[error("broadcaster version {0} outside {MIN_BROADCASTER_VERSION}..={MAX_BROADCASTER_VERSION}")]
    Version(String),
    #[error("bad signature on fee message from {0}")]
    Signature(String),
    #[error("invalid trusted fee signer \"{0}\": not a 0zk address")]
    TrustedSigner(String),
}

/// Trusted fee signers of the Railway wallet, from the `trustedFeeSigner` field of its remote
/// configuration (`https://www.railway.xyz/config/railway-config-v3.3.json`, read 2026-09-20).
/// Railway applies the same list on every network.
///
/// Not applied by default: this is the choice of one wallet team, not a protocol constant, and
/// the list can change without notice. An application opting in trusts these keys to set the
/// reference rate.
pub const RAILWAY_TRUSTED_FEE_SIGNERS: [&str; 4] = [
    "0zk1qyjyhqjdkqd9qxusgj092ppxl92plvrk3s3cna9u73h5rwt0ghxvfrv7j6fe3z53l7lrzyqw5te7ku5v8fsrpeadzvpkudgawjv9dg08htj7z3mph5kd6dw50jc",
    "0zk1qyzgh9ctuxm6d06gmax39xutjgrawdsljtv80lqnjtqp3exxayuf0rv7j6fe3z53laetcl9u3cma0q9k4npgy8c8ga4h6mx83v09m8ewctsekw4a079dcl5sw4k",
    "0zk1qyqhtwaa9zj3ug9dmxhfedappvm509w7dr5lgadaehxz38w9u457mrv7j6fe3z53layes62mktxj5kd6reh2kxd39ds2gnpf6wphtw39y5g36lsvukeywfqa8y0",
    "0zk1qy88aamk4dp3rn2dvfdu5u8vvtfs89vg8h6zyajr4g5mq0ykm28e0rv7j6fe3z53l7zpahc5w52u8juzg54ypn24slqsyy3dy57s5k3669dyg3jxyp6czxszfs7",
];

/// Identity of a 0zk address whatever its chain scope: the reference client compares address
/// strings, which misses a signer configured in chain-agnostic form and announcing in
/// chain-scoped form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SignerId(String);

impl SignerId {
    pub fn from_address(railgun_address: &str) -> Result<Self, FeeError> {
        let address = RailgunAddress::from_str(railgun_address.trim())
            .map_err(|e| FeeError::Malformed(format!("railgun address: {e}")))?;
        Ok(SignerId(format!(
            "{}{}",
            address.master_key().to_hex(),
            address.viewing_pubkey().to_hex()
        )))
    }
}

/// Trusted fee signers, `trustedFeeSigner` in the reference client.
///
/// A trusted signer's announcements define the authorized rate of each token (the average when
/// there are several signers). Any other broadcaster is only considered when its rate lies within
/// `[authorized - lower%, authorized + upper%]`, which caps what a malicious or misconfigured
/// broadcaster can charge. With a policy set and no authorized rate for a token, there is no
/// usable offer for that token.
#[derive(Debug, Clone)]
pub struct TrustPolicy {
    signers: Vec<SignerId>,
    pub lower_percent: u32,
    pub upper_percent: u32,
}

impl TrustPolicy {
    /// Reference defaults: 10% under, 30% over.
    pub fn new(signer_addresses: &[String]) -> Result<Self, FeeError> {
        let signers = signer_addresses
            .iter()
            .map(|a| SignerId::from_address(a).map_err(|_| FeeError::TrustedSigner(a.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        if signers.is_empty() {
            return Err(FeeError::TrustedSigner(String::new()));
        }
        Ok(Self {
            signers,
            lower_percent: 10,
            upper_percent: 30,
        })
    }

    pub fn signer_count(&self) -> usize {
        self.signers.len()
    }

    fn trusts(&self, signer: &SignerId) -> bool {
        self.signers.contains(signer)
    }

    /// Inclusive band around an authorized rate.
    pub fn band(&self, authorized: u128) -> (u128, u128) {
        let lower = authorized / 100 * u128::from(self.lower_percent)
            + authorized % 100 * u128::from(self.lower_percent) / 100;
        let upper = authorized / 100 * u128::from(self.upper_percent)
            + authorized % 100 * u128::from(self.upper_percent) / 100;
        (authorized - lower, authorized.saturating_add(upper))
    }
}

/// Rate at which the fee equals the gas cost, when the fee token is the wrapped base token:
/// one token base unit per wei of gas, that is 10^18 per 10^18 wei.
///
/// For that token the honest rate is known without an oracle or a trusted signer: this value
/// plus the broadcaster's margin. A ceiling expressed as a multiple of it bounds what any
/// broadcaster can charge. It means nothing for another fee token, whose rate embeds a price.
pub const PAR_RATE_WRAPPED_BASE_TOKEN: u128 = 1_000_000_000_000_000_000;

/// Why no offer could be selected.
#[derive(Debug, Error)]
pub enum NoQuote {
    #[error("no usable broadcaster offer for this fee token")]
    None,
    #[error(
        "the cheapest offer charges a rate of {cheapest}, above the ceiling of {ceiling}: \
         {rejected} offer(s) refused"
    )]
    AboveCeiling {
        cheapest: u128,
        ceiling: u128,
        rejected: usize,
    },
    #[error(
        "{offers} offer(s) require POI lists this wallet does not prove against: {required:?} \
         (wallet lists: {ours:?})"
    )]
    PoiListMismatch {
        offers: usize,
        required: Vec<String>,
        ours: Vec<String>,
    },
}

/// One broadcaster's offer for one token.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeQuote {
    pub railgun_address: String,
    pub identifier: Option<String>,
    pub fees_id: String,
    pub token: String,
    /// Token base units charged per 10^18 wei of gas cost.
    #[serde(serialize_with = "as_string")]
    pub fee_per_unit_gas: u128,
    /// Milliseconds since the epoch.
    pub expiration: u64,
    pub available_wallets: u32,
    pub relay_adapt: String,
    pub required_poi_list_keys: Vec<String>,
    pub reliability: f64,
    pub version: String,
}

fn as_string<S: serde::Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

impl FeeQuote {
    pub fn viewing_public_key(&self) -> Result<[u8; 32], FeeError> {
        viewing_public_key(&self.railgun_address)
    }

    pub fn usable_at(&self, now_ms: u64) -> bool {
        self.available_wallets > 0 && self.expiration >= now_ms + MIN_TIME_TO_EXPIRY_MS
    }
}

/// Fee in token base units for a transaction, `calculateBroadcasterFeeERC20Amount`:
/// `feePerUnitGas * (gasEstimate * 1.2 * gasPrice) / 10^18`.
pub fn token_fee(fee_per_unit_gas: u128, gas_estimate: u64, gas_price: u128) -> Option<u128> {
    let gas_limit = u128::from(gas_estimate).checked_mul(GAS_LIMIT_NUM)? / GAS_LIMIT_DEN;
    let maximum_gas = gas_limit.checked_mul(gas_price)?;
    // u128 overflows for large fee rates: go through 256 bits by hand.
    mul_div(fee_per_unit_gas, maximum_gas, 10u128.pow(18))
}

/// `a * b / d` without intermediate overflow, `None` if the result does not fit.
fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    if let Some(p) = a.checked_mul(b) {
        return Some(p / d);
    }
    // (q1*d + r1) * b / d = q1*b + r1*b/d, with r1 < d = 1e18 so r1*b may still overflow for
    // b >= 2^68; split b the same way.
    let (q1, r1) = (a / d, a % d);
    let (q2, r2) = (b / d, b % d);
    let high = q1.checked_mul(b)?;
    let mid = r1.checked_mul(q2)?;
    let low = r1.checked_mul(r2)? / d;
    high.checked_add(mid)?.checked_add(low)
}

pub fn now_ms() -> u64 {
    crate::time::SystemTime::now()
        .duration_since(crate::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn viewing_public_key(railgun_address: &str) -> Result<[u8; 32], FeeError> {
    let address = RailgunAddress::from_str(railgun_address)
        .map_err(|e| FeeError::Malformed(format!("railgun address: {e}")))?;
    hex::decode(address.viewing_pubkey().to_hex())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| FeeError::Malformed("viewing key".into()))
}

/// Parses and authenticates the payload of a fees message.
pub fn parse_fee_message(payload: &[u8], now_ms: u64) -> Result<FeeMessageData, FeeError> {
    let message: FeeMessage =
        serde_json::from_slice(payload).map_err(|e| FeeError::Malformed(e.to_string()))?;
    let json = hex::decode(strip0x(&message.data)).map_err(|e| FeeError::Malformed(e.to_string()))?;
    let data: FeeMessageData =
        serde_json::from_slice(&json).map_err(|e| FeeError::Malformed(e.to_string()))?;

    if data.fee_expiration + MAX_ANNOUNCEMENT_AGE_MS < now_ms {
        return Err(FeeError::Stale);
    }
    if !version_in_range(&data.version) {
        return Err(FeeError::Version(data.version));
    }
    let key = viewing_public_key(&data.railgun_address)?;
    match crypto::verify_signature(&message.signature, &message.data, &key) {
        Ok(true) => Ok(data),
        _ => Err(FeeError::Signature(data.railgun_address)),
    }
}

fn version_in_range(version: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let mut it = v.trim().split('.').map(|p| p.parse::<u64>().ok());
        Some((it.next()??, it.next()??, it.next()??))
    };
    match (
        parse(version),
        parse(MIN_BROADCASTER_VERSION),
        parse(MAX_BROADCASTER_VERSION),
    ) {
        (Some(v), Some(min), Some(max)) => v >= min && v <= max,
        _ => false,
    }
}

/// Latest quote per (token, broadcaster, identifier), filtered by the trust policy if any.
#[derive(Default)]
pub struct FeeCache {
    quotes: HashMap<(String, String, Option<String>), FeeQuote>,
    policy: Option<TrustPolicy>,
    /// signer -> token -> (rate, expiration)
    authorized: HashMap<SignerId, HashMap<String, (u128, u64)>>,
}

impl FeeCache {
    pub fn with_policy(policy: TrustPolicy) -> Self {
        Self {
            policy: Some(policy),
            ..Default::default()
        }
    }

    pub fn policy(&self) -> Option<&TrustPolicy> {
        self.policy.as_ref()
    }

    /// Authorized rate of a token: average over the trusted signers whose announcement is still
    /// usable. `None` without a policy or without a live announcement.
    pub fn authorized_fee(&self, token: &str, now_ms: u64) -> Option<u128> {
        self.policy.as_ref()?;
        let token = token.to_lowercase();
        let rates: Vec<u128> = self
            .authorized
            .values()
            .filter_map(|tokens| tokens.get(&token))
            .filter(|(_, expiration)| *expiration >= now_ms + MIN_TIME_TO_EXPIRY_MS)
            .map(|(rate, _)| *rate)
            .collect();
        if rates.is_empty() {
            return None;
        }
        // Sum of at most a handful of rates: divide first to stay clear of overflow.
        let n = rates.len() as u128;
        Some(rates.iter().map(|r| r / n).sum::<u128>() + rates.iter().map(|r| r % n).sum::<u128>() / n)
    }

    /// All authorized rates, for display.
    pub fn authorized_fees(&self, now_ms: u64) -> Vec<(String, u128)> {
        let mut tokens: Vec<&String> = self.authorized.values().flat_map(|t| t.keys()).collect();
        tokens.sort();
        tokens.dedup();
        tokens
            .into_iter()
            .filter_map(|t| Some((t.clone(), self.authorized_fee(t, now_ms)?)))
            .collect()
    }

    fn in_band(&self, token: &str, rate: u128, now_ms: u64) -> bool {
        let Some(policy) = &self.policy else {
            return true;
        };
        match self.authorized_fee(token, now_ms) {
            Some(authorized) => {
                let (min, max) = policy.band(authorized);
                rate >= min && rate <= max
            }
            None => false,
        }
    }

    pub fn insert(&mut self, data: FeeMessageData) {
        let now = now_ms();
        let trusted = match &self.policy {
            Some(policy) => SignerId::from_address(&data.railgun_address)
                .map(|id| policy.trusts(&id).then_some(id))
                .unwrap_or(None),
            None => None,
        };

        let rates: Vec<(String, u128)> = data
            .fees
            .iter()
            .filter_map(|(token, fee)| {
                let rate = u128::from_str_radix(strip0x(fee), 16).ok();
                if rate.is_none() {
                    debug!("unparsable fee {fee} for {token}");
                }
                Some((token.to_lowercase(), rate?))
            })
            .collect();

        // A trusted signer first moves the authorized rates, then is an ordinary broadcaster.
        if let Some(id) = &trusted {
            if data.fee_expiration >= now + MIN_TIME_TO_EXPIRY_MS {
                let entry = self.authorized.entry(id.clone()).or_default();
                for (token, rate) in &rates {
                    let newer = entry
                        .get(token)
                        .is_none_or(|(_, expiration)| *expiration < data.fee_expiration);
                    if newer {
                        entry.insert(token.clone(), (*rate, data.fee_expiration));
                    }
                }
            }
        }

        for (token, fee_per_unit_gas) in rates {
            if trusted.is_none() && !self.in_band(&token, fee_per_unit_gas, now) {
                if self.policy.is_some() {
                    debug!(
                        "offer of {} for {token} outside the authorized band, dropped",
                        data.railgun_address
                    );
                }
                continue;
            }
            self.quotes.insert(
                (token.clone(), data.railgun_address.clone(), data.identifier.clone()),
                FeeQuote {
                    railgun_address: data.railgun_address.clone(),
                    identifier: data.identifier.clone(),
                    fees_id: data.fees_id.clone(),
                    token,
                    fee_per_unit_gas,
                    expiration: data.fee_expiration,
                    available_wallets: data.available_wallets,
                    relay_adapt: data.relay_adapt.clone(),
                    required_poi_list_keys: data.required_poi_list_keys.clone(),
                    reliability: data.reliability,
                    version: data.version.clone(),
                },
            );
        }
    }

    pub fn prune(&mut self, now_ms: u64) {
        self.quotes.retain(|_, q| q.expiration > now_ms);
        for tokens in self.authorized.values_mut() {
            tokens.retain(|_, (_, expiration)| *expiration > now_ms);
        }
    }

    /// Usable quotes for `token`, cheapest first, most reliable first among equals.
    ///
    /// `our_list_keys` are the POI lists the wallet can prove against: a broadcaster requiring
    /// another list is skipped, the request would be refused.
    pub fn quotes_for(&self, token: &str, our_list_keys: &[String], now_ms: u64) -> Vec<FeeQuote> {
        let token = token.to_lowercase();
        let mut quotes: Vec<FeeQuote> = self
            .quotes
            .values()
            .filter(|q| q.token == token && q.usable_at(now_ms))
            // The authorized rate moves: an offer in band on arrival may be out of it now.
            .filter(|q| self.in_band(&token, q.fee_per_unit_gas, now_ms))
            .filter(|q| {
                q.required_poi_list_keys
                    .iter()
                    .all(|k| our_list_keys.contains(k))
            })
            .cloned()
            .collect();
        quotes.sort_by(|a, b| {
            a.fee_per_unit_gas
                .cmp(&b.fee_per_unit_gas)
                .then(b.reliability.total_cmp(&a.reliability))
        });
        quotes
    }

    /// Cheapest usable offer whose rate does not exceed `max_rate`. The error tells a market
    /// with no offer from one where every offer is too expensive.
    pub fn best_quote(
        &self,
        token: &str,
        our_list_keys: &[String],
        max_rate: Option<u128>,
        now_ms: u64,
    ) -> Result<FeeQuote, NoQuote> {
        let quotes = self.quotes_for(token, our_list_keys, now_ms);
        let cheapest = quotes.first().map(|q| q.fee_per_unit_gas);
        let total = quotes.len();
        let ceiling = max_rate.unwrap_or(u128::MAX);
        // Sorted cheapest first: the first one under the ceiling is the best.
        match quotes.into_iter().find(|q| q.fee_per_unit_gas <= ceiling) {
            Some(quote) => Ok(quote),
            None => match cheapest {
                Some(cheapest) => Err(NoQuote::AboveCeiling {
                    cheapest,
                    ceiling,
                    rejected: total,
                }),
                None => {
                    // Offers that only the POI list filter removed?
                    let mut required: Vec<String> = self
                        .quotes
                        .values()
                        .filter(|q| q.token == token.to_lowercase() && q.usable_at(now_ms))
                        .filter(|q| self.in_band(&q.token, q.fee_per_unit_gas, now_ms))
                        .map(|q| q.required_poi_list_keys.clone())
                        .flatten()
                        .filter(|k| !our_list_keys.contains(k))
                        .collect();
                    if required.is_empty() {
                        return Err(NoQuote::None);
                    }
                    let offers = required.len();
                    required.sort();
                    required.dedup();
                    Err(NoQuote::PoiListMismatch {
                        offers,
                        required,
                        ours: our_list_keys.to_vec(),
                    })
                }
            },
        }
    }

    /// Picks one offer among those within `within_percent` of the cheapest usable one, at
    /// random, as the reference client does (`findRandomBroadcasterForToken`): always taking the
    /// cheapest sends every request to the same broadcaster, however unreliable. `exclude` lists
    /// 0zk addresses to skip, typically broadcasters that just failed to answer; they are used
    /// again only if nobody else is left.
    pub fn select_quote(
        &self,
        token: &str,
        our_list_keys: &[String],
        max_rate: Option<u128>,
        within_percent: u32,
        exclude: &[String],
        pick: impl FnOnce(usize) -> usize,
        now_ms: u64,
    ) -> Result<FeeQuote, NoQuote> {
        // Reports the right reason when nothing is usable at all.
        self.best_quote(token, our_list_keys, max_rate, now_ms)?;
        let ceiling = max_rate.unwrap_or(u128::MAX);
        let usable: Vec<FeeQuote> = self
            .quotes_for(token, our_list_keys, now_ms)
            .into_iter()
            .filter(|q| q.fee_per_unit_gas <= ceiling)
            .collect();
        let preferred: Vec<FeeQuote> = usable
            .iter()
            .filter(|q| !exclude.contains(&q.railgun_address))
            .cloned()
            .collect();
        let pool = if preferred.is_empty() { usable } else { preferred };
        let cheapest = pool[0].fee_per_unit_gas;
        let threshold = cheapest.saturating_add(cheapest / 100 * u128::from(within_percent));
        let eligible: Vec<FeeQuote> = pool
            .into_iter()
            .filter(|q| q.fee_per_unit_gas <= threshold)
            .collect();
        let index = pick(eligible.len()).min(eligible.len() - 1);
        Ok(eligible[index].clone())
    }

    pub fn all(&self, now_ms: u64) -> Vec<FeeQuote> {
        let mut quotes: Vec<FeeQuote> = self
            .quotes
            .values()
            .filter(|q| q.expiration > now_ms)
            .cloned()
            .collect();
        quotes.sort_by(|a, b| (&a.token, a.fee_per_unit_gas).cmp(&(&b.token, b.fee_per_unit_gas)));
        quotes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_fee_matches_reference_formula() {
        // 300k gas estimate, 2 gwei, 0.5 token per ETH of gas (fee rate 5e17).
        let fee = token_fee(500_000_000_000_000_000, 300_000, 2_000_000_000).unwrap();
        // gas limit 360000 * 2e9 = 7.2e14 wei; * 5e17 / 1e18 = 3.6e14
        assert_eq!(fee, 360_000_000_000_000);
    }

    #[test]
    fn token_fee_survives_u128_overflow() {
        // Fee rate of a low-value 18-decimal token: 3e24 per ETH of gas.
        let rate = 3_000_000_000_000_000_000_000_000u128;
        let fee = token_fee(rate, 1_500_000, 80_000_000_000).unwrap();
        // 1.8e6 * 8e10 = 1.44e17 wei; * 3e24 / 1e18 = 4.32e23
        assert_eq!(fee, 432_000_000_000_000_000_000_000);
    }

    #[test]
    fn band_follows_the_reference_rounding() {
        let me = "0zk1qyk9nn28x0u3rwn5pknglda68wrn7gw6anjw8gg94mcj6eq5u48tlrv7j6fe3z53lama02nutwtcqc979wnce0qwly4y7w4rls5cq040g7z8eagshxrw5ajy990";
        let policy = TrustPolicy::new(&[me.to_string()]).unwrap();
        assert_eq!(policy.band(1_000), (900, 1_300));
        // floor(1999 * 10 / 100) = 199, floor(1999 * 30 / 100) = 599
        assert_eq!(policy.band(1_999), (1_800, 2_598));
        assert_eq!(policy.band(u128::MAX).1, u128::MAX);
        assert!(TrustPolicy::new(&[]).is_err());
        assert!(TrustPolicy::new(&["0zk1nope".to_string()]).is_err());
    }

    /// Catches a typo in the constant: every entry must pass the bech32m checksum and the four
    /// signers must be distinct keys.
    #[test]
    fn railway_signers_are_valid_and_distinct() {
        let signers: Vec<String> = RAILWAY_TRUSTED_FEE_SIGNERS.iter().map(|s| s.to_string()).collect();
        let policy = TrustPolicy::new(&signers).unwrap();
        assert_eq!(policy.signer_count(), 4);
        let mut ids: Vec<SignerId> = signers
            .iter()
            .map(|s| SignerId::from_address(s).unwrap())
            .collect();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        ids.dedup();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn versions() {
        assert!(version_in_range("8.0.0"));
        assert!(version_in_range("8.12.3"));
        assert!(!version_in_range("7.9.9"));
        assert!(!version_in_range("9.0.0"));
        assert!(!version_in_range("garbage"));
    }
}

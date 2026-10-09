//! Credentials accepted by the viewer, shared by the native daemon and the web build.
//!
//! * a BIP-39 mnemonic: both keys are derived (`crate::wallet_keys`, the wallet's derivation file,
//!   same schemes as the wallet);
//! * a viewing private key, with or without the 0zk address: view-only. The master public key is
//!   read from the address when given, otherwise recovered on chain
//!   (`RailgunBuilder::discover_master_key`).

use anyhow::{Context, Result, anyhow, bail};
use railgun::{
    account::{address::RailgunAddress, chain::ChainId},
    crypto::keys::{HexKey, MasterPublicKey, SpendingKey, ViewingKey},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::wallet_keys::{self, Derivation};

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    pub mnemonic: Option<String>,
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub derivation: Derivation,
    pub viewing_key: Option<String>,
}

pub enum Resolved {
    Full {
        spending: SpendingKey,
        viewing: ViewingKey,
        scheme: &'static str,
    },
    /// Viewing key only: the master public key comes from the 0zk address or from the chain.
    ViewOnly { viewing: ViewingKey },
}

impl Resolved {
    pub fn mode(&self) -> &'static str {
        match self {
            Resolved::Full { .. } => "full",
            Resolved::ViewOnly { .. } => "view-only",
        }
    }
}

pub fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn parse32(s: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(s.trim().trim_start_matches("0x"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("expected 32 bytes of hex"))
}

pub fn resolve(c: &Credentials) -> Result<Resolved> {
    if let Some(m) = non_empty(&c.mnemonic) {
        let keys = wallet_keys::derive(m, c.index, c.derivation)?;
        return Ok(Resolved::Full {
            spending: keys.spending,
            viewing: keys.viewing,
            scheme: match c.derivation {
                Derivation::Railgun => "railgun",
                Derivation::Kohaku => "kohaku",
            },
        });
    }
    if let Some(v) = non_empty(&c.viewing_key) {
        let viewing = ViewingKey::from_hex(&hex::encode(parse32(v).context("viewing key")?))
            .map_err(|e| anyhow!("viewing key: {e}"))?;
        return Ok(Resolved::ViewOnly { viewing });
    }
    bail!("provide a mnemonic or a private viewing key")
}

/// Length of a 0zk address: `0zk` + `1` + 117 data characters (73 bytes) + 6 checksum characters.
const ADDRESS_LEN: usize = 127;

/// `RailgunAddress::from_str` slices the bech32 payload without a length check: a well-formed
/// bech32 string with a short payload would panic. The length is checked first (the web build
/// aborts on panic), `catch_unwind` stays as a second guard on the native build.
pub fn parse_address(s: &str) -> Result<RailgunAddress> {
    let s = s.trim().to_ascii_lowercase();
    if s.len() != ADDRESS_LEN || !s.starts_with("0zk1") {
        bail!("0zk address: expected {ADDRESS_LEN} characters starting with 0zk1, got {}", s.len());
    }
    match std::panic::catch_unwind(|| s.parse::<RailgunAddress>()) {
        Ok(Ok(a)) => Ok(a),
        Ok(Err(e)) => Err(anyhow!("0zk address: {e}")),
        Err(_) => Err(anyhow!("0zk address: malformed payload")),
    }
}

/// Master public key read from the 0zk address, accepted only if the address's viewing public
/// key is the one of the private viewing key entered.
pub fn master_from_address(address: &str, viewing: &ViewingKey) -> Result<MasterPublicKey> {
    let a = parse_address(address)?;
    if a.viewing_pubkey() != viewing.public_key() {
        bail!(
            "this private viewing key does not belong to that 0zk address (viewing public keys differ)"
        );
    }
    Ok(a.master_key())
}

/// `POST /api/derive`: viewing private key and 0zk address derived from a mnemonic, without
/// touching the engine. Sepolia test seeds only, as the form says.
pub fn derive_display(mnemonic: &str, derivation: Derivation, index: u32, chain_id: Option<u64>) -> Value {
    match wallet_keys::derive(mnemonic.trim(), index, derivation) {
        Ok(keys) => {
            let address = chain_id.map(|c| {
                RailgunAddress::from_private_keys(keys.spending, keys.viewing, ChainId::evm(c)).to_string()
            });
            json!({ "ok": true, "viewingKey": format!("0x{}", keys.viewing.to_hex()), "address": address })
        }
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

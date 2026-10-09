//! Credentials accepted by the viewer, shared by the native daemon and the web build.
//!
//! * a BIP-39 mnemonic: both keys are derived (`crate::wallet_keys`, the wallet's derivation file,
//!   same schemes as the wallet);
//! * a viewing private key with the 0zk address: view-only, the master public key is read from the
//!   address (a bare viewing key is refused, ADR-016);
//! * the legacy format of Railway and the community engine (shareable viewing key: hex of a
//!   msgpack map `{vpriv, spub}`): view-only, with the real spending public key, hence the master
//!   public key computed exactly.

use anyhow::{Context, Result, anyhow, bail};
use ark_bn254::Fr;
use railgun::{
    account::{address::RailgunAddress, chain::ChainId},
    crypto::keys::{HexKey, MasterPublicKey, SpendingKey, SpendingPublicKey, ViewingKey},
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
    /// Legacy format (shareable viewing key), see [`parse_shareable`].
    #[serde(default)]
    pub shareable_key: Option<String>,
}

pub enum Resolved {
    Full {
        spending: SpendingKey,
        viewing: ViewingKey,
        scheme: &'static str,
    },
    /// Viewing key only: the master public key comes from the 0zk address or from the chain.
    ViewOnly { viewing: ViewingKey },
    /// Legacy format: viewing key and spending public key.
    Shared { viewing: ViewingKey, spub: SpendingPublicKey },
}

impl Resolved {
    pub fn mode(&self) -> &'static str {
        match self {
            Resolved::Full { .. } => "full",
            Resolved::ViewOnly { .. } | Resolved::Shared { .. } => "view-only",
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
    if let Some(s) = non_empty(&c.shareable_key) {
        let (viewing, spub) = parse_shareable(s)?;
        return Ok(Resolved::Shared { viewing, spub });
    }
    if let Some(v) = non_empty(&c.viewing_key) {
        // the legacy format pasted in the viewing key field is accepted as such
        if looks_shareable(v) {
            let (viewing, spub) = parse_shareable(v)?;
            return Ok(Resolved::Shared { viewing, spub });
        }
        let viewing = ViewingKey::from_hex(&hex::encode(parse32(v).context("viewing key")?))
            .map_err(|e| anyhow!("viewing key: {e}"))?;
        return Ok(Resolved::ViewOnly { viewing });
    }
    bail!("provide a mnemonic or a private viewing key")
}

/// Removes a label such as Railway's `Private key:` and whitespace around the hex.
fn strip_label(s: &str) -> String {
    let t = s.trim();
    let t = match t.rfind(':') {
        Some(i) => &t[i + 1..],
        None => t,
    };
    t.chars().filter(|c| !c.is_whitespace()).collect::<String>().trim_start_matches("0x").to_string()
}

/// A msgpack map in hex (`8…`), longer than a bare 32-byte key.
pub fn looks_shareable(s: &str) -> bool {
    let t = strip_label(s);
    t.len() > 64 && t.starts_with('8') && t.chars().all(|c| c.is_ascii_hexdigit())
}

/// Minimal msgpack reader for the shareable viewing key: a map of string keys to strings (hex
/// text) or binary values. Anything else is refused.
struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self.b.get(self.i..self.i + n).ok_or_else(|| anyhow!("legacy key: truncated"))?;
        self.i += n;
        Ok(s)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn len16(&mut self) -> Result<usize> {
        let n = self.take(2)?;
        Ok(usize::from(u16::from_be_bytes([n[0], n[1]])))
    }
    /// (is text, bytes)
    fn item(&mut self) -> Result<(bool, &'a [u8])> {
        let t = self.byte()?;
        let (text, n) = match t {
            0xa0..=0xbf => (true, usize::from(t & 0x1f)),
            0xd9 => (true, usize::from(self.byte()?)),
            0xda => (true, self.len16()?),
            0xc4 => (false, usize::from(self.byte()?)),
            0xc5 => (false, self.len16()?),
            _ => bail!("legacy key: unexpected msgpack type 0x{t:02x}"),
        };
        Ok((text, self.take(n)?))
    }
}

fn msgpack_map(b: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut c = Cursor { b, i: 0 };
    let head = c.byte().map_err(|_| anyhow!("legacy key: empty"))?;
    let entries = match head {
        0x80..=0x8f => usize::from(head & 0x0f),
        0xde => c.len16()?,
        _ => bail!("legacy key: not a msgpack map"),
    };
    let mut out = Vec::with_capacity(entries);
    for _ in 0..entries {
        let (is_text, k) = c.item()?;
        if !is_text {
            bail!("legacy key: non-string map key");
        }
        let (_, v) = c.item()?;
        out.push((String::from_utf8(k.to_vec()).map_err(|_| anyhow!("legacy key: bad key"))?, v.to_vec()));
    }
    if c.i != b.len() {
        bail!("legacy key: {} trailing byte(s)", b.len() - c.i);
    }
    Ok(out)
}

/// Value of the map as 32 bytes: hex text (the engine's encoding) or raw binary.
fn field32(map: &[(String, Vec<u8>)], key: &str) -> Result<[u8; 32]> {
    let v = map
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .ok_or_else(|| anyhow!("legacy key: no `{key}` field"))?;
    let bytes = if v.len() == 32 {
        v.clone()
    } else {
        let text = std::str::from_utf8(v).map_err(|_| anyhow!("legacy key: `{key}` is not text"))?;
        hex::decode(text.trim_start_matches("0x")).map_err(|_| anyhow!("legacy key: `{key}` is not hex"))?
    };
    bytes.try_into().map_err(|_| anyhow!("legacy key: `{key}` is not 32 bytes"))
}

/// Legacy format of Railway and the community engine (`generateShareableViewingKey`): hex of a
/// msgpack map `{vpriv: <viewing private key>, spub: <packed spending public key>}`. View-only:
/// it carries no spending key, whatever label the export gives it.
pub fn parse_shareable(s: &str) -> Result<(ViewingKey, SpendingPublicKey)> {
    let bytes = hex::decode(strip_label(s)).context("legacy key: not hex")?;
    let map = msgpack_map(&bytes)?;
    let vpriv = field32(&map, "vpriv")?;
    let spub = field32(&map, "spub")?;
    let viewing = ViewingKey::from_hex(&hex::encode(vpriv)).map_err(|e| anyhow!("legacy key: vpriv: {e}"))?;
    let spub = spub_from_packed(&spub)
        .ok_or_else(|| anyhow!("legacy key: spub is not a BabyJubJub point"))?;
    Ok((viewing, spub))
}

/// circomlib `packPoint` form of a spending public key (the community engine's `spub`): `y` in
/// little endian, top bit set when `x` is above `(p - 1) / 2`.
#[cfg(test)]
pub fn spub_to_packed(key: &SpendingPublicKey) -> [u8; 32] {
    use ark_ff::PrimeField;
    let x = Fr::from_be_bytes_mod_order(&key.x_u256().to_be_bytes::<32>());
    let mut out = key.y_u256().to_le_bytes::<32>();
    if x.into_bigint() > Fr::MODULUS_MINUS_ONE_DIV_TWO {
        out[31] |= 0x80;
    }
    out
}

/// Inverse of [`spub_to_packed`] (circomlib `unpackPoint` on BabyJubJub, `a = 168700`,
/// `d = 168696`). `None` when the bytes are not a point of the curve.
pub fn spub_from_packed(packed: &[u8; 32]) -> Option<SpendingPublicKey> {
    use ark_ff::{BigInteger, Field, One, PrimeField};
    let mut le = *packed;
    let negative = le[31] & 0x80 != 0;
    le[31] &= 0x7f;
    let y = Fr::from_le_bytes_mod_order(&le);
    let mut canon = y.into_bigint().to_bytes_le();
    canon.resize(32, 0);
    if canon[..] != le[..] {
        return None; // y >= p
    }
    let y2 = y.square();
    let den = Fr::from(168_700u64) - Fr::from(168_696u64) * y2;
    let x2 = (Fr::one() - y2) * den.inverse()?;
    let mut x = x2.sqrt()?;
    if x.into_bigint() > Fr::MODULUS_MINUS_ONE_DIV_TWO {
        x = -x;
    }
    if negative {
        x = -x;
    }
    let to32 = |f: Fr| {
        let b = f.into_bigint().to_bytes_be();
        let mut o = [0u8; 32];
        o[32 - b.len()..].copy_from_slice(&b);
        o
    };
    Some(SpendingPublicKey::new(to32(x), to32(y)))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// circomlib `packPoint` / `unpackPoint` round trip, both signs of `x`.
    #[test]
    fn spending_public_key_packing() {
        let mut negative = 0;
        for i in 1u8..=40 {
            let pk = SpendingKey::from_hex(&hex::encode([i; 32])).unwrap().public_key();
            let packed = spub_to_packed(&pk);
            negative += usize::from(packed[31] & 0x80 != 0);
            assert_eq!(spub_from_packed(&packed), Some(pk), "key {i}");
        }
        assert!(negative > 0 && negative < 40, "both signs exercised");
        // y >= p is refused
        assert_eq!(spub_from_packed(&[0x7f; 32]), None);
    }

    /// Encodes like the community engine: msgpack `{vpriv, spub}` with hex strings, as hex.
    fn shareable(vpriv: [u8; 32], spub: [u8; 32]) -> String {
        let mut b = vec![0x82];
        for (k, v) in [("vpriv", hex::encode(vpriv)), ("spub", hex::encode(spub))] {
            b.push(0xa0 | k.len() as u8);
            b.extend_from_slice(k.as_bytes());
            b.push(0xd9);
            b.push(v.len() as u8);
            b.extend_from_slice(v.as_bytes());
        }
        hex::encode(b)
    }

    #[test]
    fn legacy_key_round_trip() {
        let spending = SpendingKey::from_hex(&hex::encode([7u8; 32])).unwrap();
        let vpriv = [9u8; 32];
        let s = shareable(vpriv, spub_to_packed(&spending.public_key()));
        assert!(looks_shareable(&s));
        let (viewing, spub) = parse_shareable(&format!("Private key: {s}")).unwrap();
        assert_eq!(viewing.to_hex(), hex::encode(vpriv));
        assert_eq!(spub, spending.public_key());
        // pasted in the viewing key field
        let c = Credentials { viewing_key: Some(s.clone()), ..Default::default() };
        assert!(matches!(resolve(&c).unwrap(), Resolved::Shared { .. }));
        // a bare viewing key is not taken for the legacy format
        assert!(!looks_shareable(&hex::encode(vpriv)));
        assert!(parse_shareable(&s[..s.len() - 2]).is_err());
    }
}

//! Cryptography of the wallet to broadcaster channel.
//!
//! This is not the note encryption scheme of `railgun::crypto`: the Railgun wallet SDK uses
//! `@noble/ed25519` `getSharedSecret` here, an X25519 exchange on the Montgomery form of the
//! ed25519 keys, and the raw 32-byte output is the AES key (no hash).

use aes_gcm::{
    AesGcm,
    aead::{AeadInPlace, KeyInit, consts::U16},
};
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use rand::{CryptoRng, RngExt};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha512};
use thiserror::Error;

/// AES-256-GCM with the 16-byte nonce the Railgun engine uses.
type Aes256Gcm16 = AesGcm<aes_gcm::aes::Aes256, U16>;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("invalid ed25519 public key")]
    InvalidPublicKey,
    #[error("invalid hex: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("malformed encrypted data")]
    Malformed,
    #[error("decryption failed, the message is not for this key")]
    Decrypt,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// `[iv || tag, ciphertext]`, both `0x`-prefixed hex: the engine's `EncryptedData`.
pub type EncryptedData = [String; 2];

/// An ed25519 key pair used once, for one transact request.
pub struct EphemeralKey {
    secret: [u8; 32],
    public: [u8; 32],
}

impl EphemeralKey {
    pub fn generate(rng: &mut impl CryptoRng) -> Self {
        Self::from_secret(rng.random())
    }

    pub fn from_secret(secret: [u8; 32]) -> Self {
        let public = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
        Self { secret, public }
    }

    /// Public key as the broadcaster expects it in `params.pubkey`: hex, no prefix.
    pub fn public_hex(&self) -> String {
        hex::encode(self.public)
    }

    pub fn shared_secret(&self, their_public: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
        shared_secret(&self.secret, their_public)
    }
}

/// `@noble/ed25519` v1 `getSharedSecret(privateKey, publicKey)`.
pub fn shared_secret(secret: &[u8; 32], their_public: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
    let point = CompressedEdwardsY(*their_public)
        .decompress()
        .ok_or(CryptoError::InvalidPublicKey)?;
    let mut head = [0u8; 32];
    head.copy_from_slice(&Sha512::digest(secret)[..32]);
    Ok(point.to_montgomery().mul_clamped(head).to_bytes())
}

/// Encrypts a JSON value the way the engine's `encryptJSONDataWithSharedKey` does.
pub fn seal_json<T: Serialize>(
    value: &T,
    key: &[u8; 32],
    rng: &mut impl CryptoRng,
) -> Result<EncryptedData, CryptoError> {
    let iv: [u8; 16] = rng.random();
    let mut buffer = serde_json::to_vec(value)?;
    let cipher = Aes256Gcm16::new(key.into());
    let tag = cipher
        .encrypt_in_place_detached((&iv).into(), &[], &mut buffer)
        .map_err(|_| CryptoError::Malformed)?;
    Ok([
        format!("0x{}{}", hex::encode(iv), hex::encode(tag)),
        format!("0x{}", hex::encode(buffer)),
    ])
}

/// Inverse of [`seal_json`]. `Err(Decrypt)` means the message was sealed for someone else.
pub fn open_json<T: DeserializeOwned>(
    data: &EncryptedData,
    key: &[u8; 32],
) -> Result<T, CryptoError> {
    let iv_tag = hex::decode(strip0x(&data[0]))?;
    if iv_tag.len() != 32 {
        return Err(CryptoError::Malformed);
    }
    let (iv, tag) = iv_tag.split_at(16);
    let mut buffer = hex::decode(strip0x(&data[1]))?;
    let cipher = Aes256Gcm16::new(key.into());
    cipher
        .decrypt_in_place_detached(iv.into(), &[], &mut buffer, tag.into())
        .map_err(|_| CryptoError::Decrypt)?;
    Ok(serde_json::from_slice(&buffer)?)
}

/// Verifies the ed25519 signature of a fee message. `data_hex` is signed as its decoded bytes.
pub fn verify_signature(
    signature_hex: &str,
    data_hex: &str,
    public: &[u8; 32],
) -> Result<bool, CryptoError> {
    let key = VerifyingKey::from_bytes(public).map_err(|_| CryptoError::InvalidPublicKey)?;
    let signature = hex::decode(strip0x(signature_hex))?;
    let Ok(signature) = Signature::from_slice(&signature) else {
        return Ok(false);
    };
    let message = hex::decode(strip0x(data_hex))?;
    Ok(key.verify(&message, &signature).is_ok())
}

pub(crate) fn strip0x(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    // Vectors produced with @noble/ed25519 1.7.3 and Node's aes-256-gcm (16-byte IV), the
    // libraries the Railgun wallet SDK and broadcasters run on.
    const CLIENT_SECRET: [u8; 32] = [0x11; 32];
    const BROADCASTER_SECRET: [u8; 32] = [0x22; 32];
    const CLIENT_PUB: &str = "d04ab232742bb4ab3a1368bd4615e4e6d0224ab71a016baf8520a332c9778737";
    const BROADCASTER_PUB: &str = "a09aa5f47a6759802ff955f8dc2d2a14a5c99d23be97f864127ff9383455a4f0";
    const SHARED: &str = "e4f89e666efa723bce776c3df12d9313a2416965d9acc2279388230b8a136262";

    fn key(hex_str: &str) -> [u8; 32] {
        hex::decode(hex_str).unwrap().try_into().unwrap()
    }

    #[test]
    fn shared_secret_matches_noble() {
        let client = EphemeralKey::from_secret(CLIENT_SECRET);
        assert_eq!(client.public_hex(), CLIENT_PUB);
        assert_eq!(
            hex::encode(client.shared_secret(&key(BROADCASTER_PUB)).unwrap()),
            SHARED
        );
        assert_eq!(
            hex::encode(shared_secret(&BROADCASTER_SECRET, &key(CLIENT_PUB)).unwrap()),
            SHARED
        );
    }

    #[test]
    fn opens_engine_ciphertext() {
        let data = [
            "0x33333333333333333333333333333333c3331e92cf3d294d1f668a2f976291c3".to_string(),
            "0xd2c465eb9c7183466e849a1187032629e3ac5584ce9d11a33914bd".to_string(),
        ];
        let value: Value = open_json(&data, &key(SHARED)).unwrap();
        assert_eq!(value, json!({ "id": "x", "txHash": "0xabc" }));
        assert!(matches!(
            open_json::<Value>(&data, &[0u8; 32]),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn seal_round_trip() {
        let sealed = seal_json(&json!({ "a": 1 }), &key(SHARED), &mut rand::rng()).unwrap();
        assert_eq!(sealed[0].len(), 2 + 64);
        let value: Value = open_json(&sealed, &key(SHARED)).unwrap();
        assert_eq!(value, json!({ "a": 1 }));
    }

    #[test]
    fn verifies_noble_signature() {
        let data = "7b2266656573223a7b223078616263223a2230783130227d2c22666565734944223a2266227d";
        let sig = "65f4ffd181a12e208f2b63dca905069af98daee1583f7170c829b431269f93b64db6d31332964bc9f7396fc3a52d0f90f82737ac9210c030754849a72344d802";
        assert!(verify_signature(sig, data, &key(BROADCASTER_PUB)).unwrap());
        assert!(!verify_signature(sig, data, &key(CLIENT_PUB)).unwrap());
    }
}

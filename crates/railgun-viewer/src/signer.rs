//! View-only signers. They satisfy `RailgunSigner` for the read paths of the SDK (note
//! decryption, nullifiers, addresses) and refuse to sign. `MasterSigner`'s placeholder spending
//! public key is random and only ever reaches POI proof generation, which the viewer never runs
//! (it syncs with `sync_without_poi_submission`); `SharedSigner` (legacy format) has the real one.

use std::sync::Arc;

use railgun::{
    account::{
        chain::ChainId,
        signer::{RailgunSigner, RailgunSignerError},
    },
    crypto::{
        kem::KemKeyPair,
        keys::{MasterPublicKey, SpendingKey, SpendingPublicKey, SpendingSignature, ViewingKey},
    },
};
use alloy::primitives::U256;

/// Viewing key + master public key (discovered on chain). The spending public key is unknown:
/// only the master key is used by the read paths.
pub struct MasterSigner {
    viewing: ViewingKey,
    master: MasterPublicKey,
    placeholder: SpendingKey,
    /// Derived from the viewing key, as the wallet does: post-quantum notes are readable too.
    kem: KemKeyPair,
    chain: ChainId,
}

impl MasterSigner {
    pub fn new(viewing: ViewingKey, master: MasterPublicKey, chain_id: u64) -> Arc<Self> {
        Arc::new(Self {
            viewing,
            master,
            placeholder: rand::random(),
            kem: viewing.kem_key_pair(),
            chain: ChainId::evm(chain_id),
        })
    }
}

// Same shape as the SDK trait: `Send` futures on native, `?Send` on wasm (web build).
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RailgunSigner for MasterSigner {
    fn chain_id(&self) -> ChainId {
        self.chain
    }
    fn viewing_key(&self) -> ViewingKey {
        self.viewing
    }
    /// Placeholder: the real spending public key is not known, only the master key is. Read paths
    /// go through [`Self::master_public_key`].
    fn spending_public_key(&self) -> SpendingPublicKey {
        self.placeholder.public_key()
    }
    fn master_public_key(&self) -> MasterPublicKey {
        self.master
    }
    fn kem_key_pair(&self) -> Option<&KemKeyPair> {
        Some(&self.kem)
    }
    async fn sign(&self, _inputs: U256) -> Result<SpendingSignature, RailgunSignerError> {
        Err(RailgunSignerError::new("view-only signer: no spending key"))
    }
}

/// Legacy format (shareable viewing key): viewing key and the real spending public key. The
/// master public key is the SDK's default, computed from both, so it is exact.
pub struct SharedSigner {
    viewing: ViewingKey,
    spub: SpendingPublicKey,
    kem: KemKeyPair,
    chain: ChainId,
}

impl SharedSigner {
    pub fn new(viewing: ViewingKey, spub: SpendingPublicKey, chain_id: u64) -> Arc<Self> {
        Arc::new(Self {
            viewing,
            spub,
            kem: viewing.kem_key_pair(),
            chain: ChainId::evm(chain_id),
        })
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RailgunSigner for SharedSigner {
    fn chain_id(&self) -> ChainId {
        self.chain
    }
    fn viewing_key(&self) -> ViewingKey {
        self.viewing
    }
    fn spending_public_key(&self) -> SpendingPublicKey {
        self.spub
    }
    fn kem_key_pair(&self) -> Option<&KemKeyPair> {
        Some(&self.kem)
    }
    async fn sign(&self, _inputs: U256) -> Result<SpendingSignature, RailgunSignerError> {
        Err(RailgunSignerError::new("view-only signer: no spending key"))
    }
}

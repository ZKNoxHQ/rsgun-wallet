//! View-only signers. They satisfy `RailgunSigner` for the read paths of the SDK (note
//! decryption, nullifiers, addresses) and refuse to sign. The placeholder spending public key is
//! random and only ever reaches code that is disabled in view-only mode (`with_poi_read_only`).

use std::sync::Arc;

use railgun::{
    account::{
        chain::ChainId,
        signer::{RailgunSigner, RailgunSignerError},
    },
    crypto::keys::{MasterPublicKey, SpendingKey, SpendingPublicKey, SpendingSignature, ViewingKey},
};
use alloy::primitives::U256;

/// Viewing key + master public key (discovered on chain). The spending public key is unknown:
/// only the master key is used by the read paths.
pub struct MasterSigner {
    viewing: ViewingKey,
    master: MasterPublicKey,
    placeholder: SpendingKey,
    chain: ChainId,
}

impl MasterSigner {
    pub fn new(viewing: ViewingKey, master: MasterPublicKey, chain_id: u64) -> Arc<Self> {
        Arc::new(Self {
            viewing,
            master,
            placeholder: rand::random(),
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
    async fn sign(&self, _inputs: U256) -> Result<SpendingSignature, RailgunSignerError> {
        Err(RailgunSignerError::new("view-only signer: no spending key"))
    }
}

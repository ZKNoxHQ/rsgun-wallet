//! Pre-transaction POIs: the proofs a broadcaster requires, before it relays a transaction, that
//! its inputs have valid POIs.
//!
//! It is the post-transaction POI circuit, proved by the SDK
//! ([`RailgunProvider::prove_operation_pois`]) with the two conventions of the Railgun engine for
//! an operation that is not on-chain yet:
//! - the outputs sit at the pre-inclusion position, tree and index 199,999
//!   ([`UtxoTreeIndex::pre_inclusion`]);
//! - the txid is in no tree, so its inclusion proof is a dummy one ([`dummy_txid_proof`]).

use std::collections::HashMap;

use railgun::{
    Proof,
    crypto::railgun_txid::Txid,
    merkle_tree::{MerkleProof, MerkleRoot, TREE_DEPTH, TxidLeafHash, UtxoTreeIndex},
    poi::{BlindedCommitment, ListKey},
    provider::{RailgunProvider, RailgunProviderError},
    transact::ProvedOperation,
};
use ruint::aliases::U256;
use serde::{Deserialize, Serialize};

/// Proof that the inputs of a not yet mined operation have valid POIs (`PreTransactionPOI` in
/// the Railgun shared models).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreTransactionPoi {
    #[serde(rename = "snarkProof")]
    pub proof: Proof,
    /// Root of the dummy txid proof, see [`dummy_txid_proof`].
    pub txid_merkleroot: MerkleRoot,
    pub poi_merkleroots: Vec<MerkleRoot>,
    pub blinded_commitments_out: Vec<BlindedCommitment>,
    pub railgun_txid_if_has_unshield: Txid,
}

/// Pre-transaction POIs per list key, then per txid leaf hash (64 hex digits, no prefix).
pub type PreTransactionPois = HashMap<ListKey, HashMap<String, PreTransactionPoi>>;

/// Generates, for every operation and list key, the pre-transaction POI a broadcaster asks for
/// before relaying. Fails when POI is not enabled on `railgun`.
pub async fn pre_transaction_pois(
    railgun: &RailgunProvider,
    operations: &[ProvedOperation],
) -> Result<PreTransactionPois, RailgunProviderError> {
    let mut result: PreTransactionPois = HashMap::new();
    for op in operations {
        let leaf = TxidLeafHash::new(
            Txid::from_operation(op),
            op.inner.utxo_tree_number,
            UtxoTreeIndex::pre_inclusion(),
        );
        let leaf_key = format!("{:064x}", <U256 as From<TxidLeafHash>>::from(leaf));
        let proofs = railgun
            .prove_operation_pois(op, UtxoTreeIndex::pre_inclusion(), dummy_txid_proof(leaf))
            .await?;
        for (list_key, poi) in proofs {
            result.entry(list_key).or_default().insert(
                leaf_key.clone(),
                PreTransactionPoi {
                    proof: poi.proof,
                    txid_merkleroot: poi.txid_merkleroot,
                    poi_merkleroots: poi.poi_merkleroots,
                    blinded_commitments_out: poi.blinded_commitments_out,
                    railgun_txid_if_has_unshield: poi.railgun_txid_if_has_unshield,
                },
            );
        }
    }
    Ok(result)
}

/// Inclusion proof of a txid that is in no tree yet, as the Railgun engine builds it
/// (`createDummyMerkleProof`): leaf at index 0, every sibling literally zero (not the tree's zero
/// value). Broadcasters recompute this root from the leaf.
pub fn dummy_txid_proof(leaf: TxidLeafHash) -> MerkleProof {
    let leaf = <U256 as From<TxidLeafHash>>::from(leaf);
    let root = (0..TREE_DEPTH).fold(leaf, |acc, _| {
        ::crypto::poseidon_hash(&[acc, U256::ZERO]).expect("poseidon on two field elements")
    });
    MerkleProof::new(leaf, [[U256::ZERO; 2]; TREE_DEPTH], [0; TREE_DEPTH], root)
}

#[cfg(test)]
mod tests {
    use railgun::merkle_tree::{MerkleTree, proof_indices};

    use super::*;

    #[test]
    fn dummy_txid_proof_is_a_zero_sibling_chain() {
        let leaf = TxidLeafHash::from(U256::from(1234u64));
        let proof = dummy_txid_proof(leaf);

        assert_eq!(proof.leaf, U256::from(1234u64));
        assert!(proof.siblings.iter().all(|row| *row == [U256::ZERO; 2]));
        assert_eq!(proof.path, [0; TREE_DEPTH]);
        assert_eq!(proof_indices(&proof), U256::ZERO);
        let expected = (0..TREE_DEPTH).fold(proof.leaf, |acc, _| {
            ::crypto::poseidon_hash(&[acc, U256::ZERO]).unwrap()
        });
        assert_eq!(proof.root, expected);

        // Not the root of a real tree holding the leaf: real trees pad with the zero value.
        let mut tree = MerkleTree::new(0);
        tree.insert_leaves(&[U256::from(1234u64)], 0).unwrap();
        assert_ne!(MerkleRoot::new(proof.root), tree.root().unwrap());
    }
}

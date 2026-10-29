//! Post-transaction calls through Railgun's RelayAdapt contract: unshield the wrapped base token
//! and receive the native currency in the same EVM transaction.
//!
//! `RelayAdapt.relay(transactions, actionData)` runs `RailgunSmartWallet.transact` and then the
//! calls of `actionData`. The calls are bound into every proof through the bound params:
//! `adaptContract = RelayAdapt` and
//! `adaptParams = keccak256(abi.encode(nullifiers, transactions.length, actionData))`, so the
//! submitter (a broadcaster) can neither change the recipient nor drop a call. This mirrors
//! `RelayAdaptHelper.getRelayAdaptParams` of the Railgun community engine.
//!
//! The SDK only provides the generic hook ([`Spend::adapt`]); the ERC-4337 transport does not
//! need any of this, the smart account unwraps in its execution phase.

use alloy::{
    primitives::{Address, B256, Bytes, FixedBytes, U256, keccak256},
    rpc::types::TransactionRequest,
    sol,
    sol_types::{SolCall, SolValue},
};
use railgun::transact::{ProvedTx, Spend};
use rand::CryptoRng;

sol! {
    /// Subset of Railgun's `RelayAdapt.json` ABI (V2).
    #[derive(Debug)]
    enum TokenType { ERC20, ERC721, ERC1155 }

    #[derive(Debug)]
    struct TokenData {
        TokenType tokenType;
        address tokenAddress;
        uint256 tokenSubID;
    }

    contract RelayAdapt {
        #[derive(Debug)]
        struct Call {
            address to;
            bytes data;
            uint256 value;
        }
        #[derive(Debug)]
        struct ActionData {
            bytes31 random;
            bool requireSuccess;
            uint256 minGasLimit;
            Call[] calls;
        }
        #[derive(Debug)]
        struct TokenTransfer {
            TokenData token;
            address to;
            uint256 value;
        }
        event CallError(uint256 callIndex, bytes revertReason);
        function unwrapBase(uint256 _amount) external;
        function transfer(TokenTransfer[] calldata _transfers) external;
    }
}

/// Selector of `relay(Transaction[],ActionData)`. The transaction struct is the SDK's, so the
/// call is encoded by hand: selector, then the parameters (checked against the engine below).
const RELAY_SELECTOR: [u8; 4] = [0x28, 0x22, 0x3a, 0x77];

/// The calls RelayAdapt runs after the transaction, and the parameters bound into the proofs.
#[derive(Debug, Clone)]
pub struct RelayAction {
    /// RelayAdapt contract of the chain: the adapt contract and the target of every call.
    pub relay_adapt: Address,
    /// 31 bytes of salt, so two identical actions never share an `adaptParams`.
    pub random: [u8; 31],
    /// When true the whole transaction reverts if a call fails (`sendWithPublicWallet` in the
    /// community engine); broadcaster submissions use false so the Railgun transaction itself
    /// lands even if a call reverts (the contract then emits `CallError`).
    pub require_success: bool,
    /// Minimum gas the relay call must have left before running the calls; 0 for no check.
    pub min_gas_limit: U256,
    /// Calls in execution order, all targeting `relay_adapt` for the built-in actions.
    pub calls: Vec<RelayAdapt::Call>,
}

impl RelayAction {
    /// Unshield the wrapped base token to `to` as native currency: the transaction unshields
    /// to the RelayAdapt contract, which unwraps its whole balance and transfers it. Amounts
    /// of 0 mean "everything the contract holds", exactly as the community engine populates
    /// `unshieldBaseToken`.
    pub fn unshield_base_token(relay_adapt: Address, to: Address, rng: &mut impl CryptoRng) -> Self {
        let mut random = [0u8; 31];
        rng.fill_bytes(&mut random);
        let base_token = TokenData {
            tokenType: TokenType::ERC20,
            tokenAddress: Address::ZERO,
            tokenSubID: U256::ZERO,
        };
        let calls = vec![
            RelayAdapt::Call {
                to: relay_adapt,
                data: RelayAdapt::unwrapBaseCall { _amount: U256::ZERO }.abi_encode().into(),
                value: U256::ZERO,
            },
            RelayAdapt::Call {
                to: relay_adapt,
                data: RelayAdapt::transferCall {
                    _transfers: vec![RelayAdapt::TokenTransfer {
                        token: base_token,
                        to,
                        value: U256::ZERO,
                    }],
                }
                .abi_encode()
                .into(),
                value: U256::ZERO,
            },
        ];
        Self {
            relay_adapt,
            random,
            require_success: false,
            min_gas_limit: U256::ZERO,
            calls,
        }
    }

    pub fn action_data(&self) -> RelayAdapt::ActionData {
        RelayAdapt::ActionData {
            random: FixedBytes::<31>::from(self.random),
            requireSuccess: self.require_success,
            minGasLimit: self.min_gas_limit,
            calls: self.calls.clone(),
        }
    }

    /// `adaptParams` for a set of transactions, from their nullifiers in transaction order:
    /// `keccak256(abi.encode(bytes32[][] nullifiers, uint256 transactionsLength, ActionData))`.
    /// RelayAdapt recomputes it in `relay` and rejects a transaction whose bound params differ.
    pub fn adapt_params(&self, nullifiers: &[Vec<B256>]) -> [u8; 32] {
        // abi.encode(a, b, c): the tuple is the parameter list, not one tuple-typed parameter.
        let preimage = (
            nullifiers.to_vec(),
            U256::from(nullifiers.len()),
            self.action_data(),
        )
            .abi_encode_params();
        keccak256(preimage).0
    }

    /// Binds this action into `spends`: RelayAdapt as adapt contract, and the adapt params
    /// derived from the nullifiers of their inputs, in transaction order. The inputs of a spend
    /// are fixed, so the params bound here are exactly what RelayAdapt recomputes on-chain.
    pub fn bind(&self, spends: Vec<Spend>) -> Vec<Spend> {
        let nullifiers: Vec<Vec<B256>> = spends
            .iter()
            .map(|spend| spend.inputs().iter().map(|note| note.nullifier.into()).collect())
            .collect();
        let params = self.adapt_params(&nullifiers);
        spends
            .into_iter()
            .map(|spend| spend.adapt(self.relay_adapt, params))
            .collect()
    }
}

/// The call a Railgun transaction request carries: target, calldata and value. The SDK sets
/// the three on every request it builds; a request missing one is not a Railgun transaction.
pub fn call_parts(tx: &TransactionRequest) -> Option<(Address, Bytes, U256)> {
    let to = tx.to.and_then(|kind| kind.to().copied())?;
    let data = tx.input.input().cloned().unwrap_or_default();
    Some((to, data, tx.value.unwrap_or_default()))
}

impl RelayAction {
    /// Calldata of `RelayAdapt.relay(transactions, actionData)`.
    pub fn relay_calldata<T: SolValue>(&self, transactions: Vec<T>) -> Bytes {
        let mut data = RELAY_SELECTOR.to_vec();
        data.extend((transactions, self.action_data()).abi_encode_params());
        data.into()
    }

    /// Turns a transaction proved from [`Self::bind`]-ed spends into the call to
    /// `RelayAdapt.relay` that carries it.
    pub fn package(&self, proved: &mut ProvedTx) {
        let transactions = proved
            .proved_operations
            .iter()
            .map(|op| op.transaction.clone())
            .collect();
        proved.tx_data = TransactionRequest::default()
            .to(self.relay_adapt)
            .input(self.relay_calldata(transactions).into())
            .value(U256::ZERO);
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, b256, hex};

    use super::*;

    // Vectors produced with the community engine's ABI (RelayAdapt.json, V2) through ethers:
    // `RelayAdaptHelper.getRelayAdaptParams` preimage layout and the two call encodings.
    const RELAY: Address = address!("4025ee6512DBbda97049Bcf5AA5D38C54aF6bE8a");
    const TO: Address = address!("000000000000000000000000000000000000dEaD");
    /// `relay([], actionData)` as ethers encodes it with the fixed salt.
    const RELAY_EMPTY: &str = "28223a7700000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000000011111111111111111111111111111111111111111111111111111111111111000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000001000000000000000000000000004025ee6512dbbda97049bcf5aa5d38c54af6be8a000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000024d5774a280000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004025ee6512dbbda97049bcf5aa5d38c54af6be8a0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e4c2e9ffd800000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000dead000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    fn fixed_action() -> RelayAction {
        let mut a = RelayAction::unshield_base_token(RELAY, TO, &mut rand::rng());
        a.random = [0x11; 31];
        a
    }

    #[test]
    fn unshield_base_token_calls_match_the_engine() {
        let a = fixed_action();
        assert_eq!(a.calls.len(), 2);
        assert_eq!(
            hex::encode(&a.calls[0].data),
            "d5774a280000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            hex::encode(&a.calls[1].data),
            "c2e9ffd800000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000dead0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert!(a.calls.iter().all(|c| c.to == RELAY && c.value.is_zero()));
    }

    #[test]
    fn adapt_params_match_relay_adapt_helper() {
        let a = fixed_action();
        let nullifiers = vec![
            vec![
                b256!("0101010101010101010101010101010101010101010101010101010101010101"),
                b256!("0202020202020202020202020202020202020202020202020202020202020202"),
            ],
            vec![b256!("0303030303030303030303030303030303030303030303030303030303030303")],
        ];
        assert_eq!(
            hex::encode(a.adapt_params(&nullifiers)),
            "abc633ee814bf8acd95c94c99858fb9aa8be4de2069fe3a713f1548f064c0323"
        );
        // the community preimage for these inputs is 1152 bytes
        let preimage = (nullifiers.clone(), U256::from(2), a.action_data()).abi_encode_params();
        assert_eq!(preimage.len(), 1152);
        // any change to the salt or the nullifier set changes the binding
        let mut b = a.clone();
        b.random[0] ^= 1;
        assert_ne!(b.adapt_params(&nullifiers), a.adapt_params(&nullifiers));
        assert_ne!(a.adapt_params(&nullifiers[..1]), a.adapt_params(&nullifiers));
    }

    #[test]
    fn relay_calldata_matches_the_engine() {
        let a = fixed_action();
        // No transactions: the element type does not change the encoding of an empty array.
        let data = a.relay_calldata(Vec::<U256>::new());
        assert_eq!(&data[..4], &hex!("28223a77"));
        assert_eq!(data.len(), 900);
        assert_eq!(keccak256(&data), keccak256(hex::decode(RELAY_EMPTY).unwrap()));
    }

    sol! {
        /// Same shape as `relay`, with `C` standing in for the SDK's transaction struct and `A`
        /// laid out like `ActionData`.
        contract Probe {
            struct C { address to; bytes data; uint256 value; }
            struct A { bytes31 random; bool requireSuccess; uint256 minGasLimit; C[] calls; }
            function relayLike(C[] _transactions, A _actionData);
        }
    }

    /// The hand-encoded parameters equal what a `sol!` call encodes for the same signature, on a
    /// non-empty array of dynamic structs.
    #[test]
    fn relay_calldata_encodes_like_a_sol_call() {
        let a = fixed_action();
        let to_c = |c: &RelayAdapt::Call| Probe::C {
            to: c.to,
            data: c.data.clone(),
            value: c.value,
        };
        let items: Vec<Probe::C> = a.calls.iter().map(to_c).collect();
        let ours = a.relay_calldata(items.clone());
        let reference = Probe::relayLikeCall {
            _transactions: items,
            _actionData: Probe::A {
                random: FixedBytes::<31>::from(a.random),
                requireSuccess: a.require_success,
                minGasLimit: a.min_gas_limit,
                calls: a.calls.iter().map(to_c).collect(),
            },
        }
        .abi_encode();
        assert_eq!(&ours[4..], &reference[4..]);
    }
}

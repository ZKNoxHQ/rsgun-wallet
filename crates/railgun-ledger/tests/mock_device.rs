//! End-to-end test of the signer over a mock device that implements the ZKNOX APDU
//! protocol in software. Proves the wire format round-trips: a `LedgerSigner` must be
//! indistinguishable from a `PrivateKeySigner` holding the same keys.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use railgun::{
    account::{
        chain::ChainId,
        signer::{PrivateKeySigner, RailgunSigner},
    },
    crypto::keys::{HexKey, SpendingKey, ViewingKey},
};
use railgun_ledger::{
    Apdu, ApduResponse, Exchange, LedgerSigner, TransportError,
    protocol::{
        CLA, INS_BLIND_SIGN, INS_GET_VERSION, INS_RAILGUN_ADDRESS, INS_SPENDING_PUBKEY,
        INS_VIEWING_PRIVKEY, INS_VIEWING_PUBKEY, P1_CURVE_BABYJUBJUB, P1_DISPLAY,
    },
};
use ruint::aliases::U256;

const SPENDING_HEX: &str = "039b3b11110e49d7340cbe7171791972e3c0d94ef31b18d6ab93d7ace62d278a";
const VIEWING_HEX: &str = "d345b2cc2f414aa93413b9572fa2b26e0e869e9274b006415a8d62ab1fa2dcb1";
const ACCOUNT_INDEX: u32 = 0;

/// A software model of the device app: checks the APDU framing byte-for-byte and answers
/// with the same key material a real device would hold.
struct MockDevice {
    spending: SpendingKey,
    viewing: ViewingKey,
    deny_signing: bool,
    sign_with_wrong_key: bool,
    /// Simulates a handle gone stale (the device re-enumerated): the next signing exchange
    /// fails at the transport level until [`Exchange::reconnect`] is called.
    stale_handle: AtomicBool,
    reconnects: AtomicUsize,
    spending_pubkey_requests: AtomicUsize,
}

impl MockDevice {
    fn new() -> Self {
        Self {
            spending: SpendingKey::from_hex(SPENDING_HEX).unwrap(),
            viewing: ViewingKey::from_hex(VIEWING_HEX).unwrap(),
            deny_signing: false,
            sign_with_wrong_key: false,
            stale_handle: AtomicBool::new(false),
            reconnects: AtomicUsize::new(0),
            spending_pubkey_requests: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Exchange for MockDevice {
    async fn exchange(&self, apdu: &Apdu) -> Result<ApduResponse, TransportError> {
        assert_eq!(apdu.cla, CLA);
        assert_eq!(apdu.p2, 0);

        let ok = |data: Vec<u8>| {
            Ok(ApduResponse {
                data,
                status: 0x9000,
            })
        };
        match apdu.ins {
            INS_GET_VERSION => {
                assert!(apdu.data.is_empty());
                ok(vec![0, 1, 0])
            }
            INS_SPENDING_PUBKEY => {
                // The silent form is debug-only in the app: the host must display.
                assert_eq!(apdu.p1, P1_DISPLAY);
                assert_eq!(apdu.data, ACCOUNT_INDEX.to_be_bytes());
                self.spending_pubkey_requests.fetch_add(1, Ordering::SeqCst);
                let pubkey = self.spending.public_key();
                let mut data = pubkey.x_u256().to_be_bytes::<32>().to_vec();
                data.extend_from_slice(&pubkey.y_u256().to_be_bytes::<32>());
                ok(data)
            }
            INS_VIEWING_PUBKEY => {
                // Production firmware requires display-and-confirm here too.
                assert_eq!(apdu.p1, P1_DISPLAY);
                assert_eq!(apdu.data, ACCOUNT_INDEX.to_be_bytes());
                ok(hex::decode(self.viewing.public_key().to_hex()).unwrap())
            }
            INS_VIEWING_PRIVKEY => {
                assert_eq!(apdu.p1, 0);
                assert_eq!(apdu.data, ACCOUNT_INDEX.to_be_bytes());
                ok(hex::decode(self.viewing.to_hex()).unwrap())
            }
            INS_BLIND_SIGN => {
                assert_eq!(apdu.p1, P1_CURVE_BABYJUBJUB);
                if self.stale_handle.load(Ordering::SeqCst) {
                    return Err(TransportError("device disconnected".into()));
                }
                if self.deny_signing {
                    return Ok(ApduResponse {
                        data: Vec::new(),
                        status: 0x6985,
                    });
                }
                assert_eq!(apdu.data.len(), 36);
                assert_eq!(&apdu.data[..4], ACCOUNT_INDEX.to_be_bytes());
                // The wire message is little-endian, as the real firmware consumes it.
                let hash = U256::from_le_slice(&apdu.data[4..]);
                let key = if self.sign_with_wrong_key {
                    SpendingKey::from_hex(&hex::encode([7u8; 32])).unwrap()
                } else {
                    self.spending
                };
                let signature = key.sign(hash);
                // Firmware layout: sigLen(1) || R8x || R8y || S || echoed msg_hash(32).
                let mut data = vec![96u8];
                data.extend_from_slice(&signature.r8_x.to_be_bytes::<32>());
                data.extend_from_slice(&signature.r8_y.to_be_bytes::<32>());
                data.extend_from_slice(&signature.s.to_be_bytes::<32>());
                data.extend_from_slice(&apdu.data[4..]);
                ok(data)
            }
            INS_RAILGUN_ADDRESS => {
                assert_eq!(apdu.p1, P1_DISPLAY);
                assert_eq!(apdu.data, ACCOUNT_INDEX.to_be_bytes());
                let address = PrivateKeySigner::new(self.spending, self.viewing, ChainId::All)
                    .address()
                    .to_string();
                assert_eq!(address.len(), 127);
                ok(address.into_bytes())
            }
            ins => panic!("unexpected instruction {ins:#04x}"),
        }
    }

    async fn reconnect(&self) -> Result<(), TransportError> {
        self.reconnects.fetch_add(1, Ordering::SeqCst);
        self.stale_handle.store(false, Ordering::SeqCst);
        Ok(())
    }
}

fn software_signer() -> std::sync::Arc<PrivateKeySigner> {
    PrivateKeySigner::new(
        SpendingKey::from_hex(SPENDING_HEX).unwrap(),
        ViewingKey::from_hex(VIEWING_HEX).unwrap(),
        ChainId::All,
    )
}

#[tokio::test]
async fn ledger_signer_matches_software_signer() {
    let ledger = LedgerSigner::connect(MockDevice::new(), ChainId::All, ACCOUNT_INDEX)
        .await
        .unwrap();
    let software = software_signer();

    // Same address, hence same master and viewing public keys.
    assert_eq!(ledger.address(), software.address());

    // The device's displayed 0zk string matches the host derivation.
    let shown = ledger.verify_address_on_device().await.unwrap();
    assert_eq!(shown, software.address().to_string());

    // Deterministic EdDSA: the signature through the APDU layer is bit-identical.
    let message = U256::from(42u64);
    let from_device = ledger.sign(message).await.unwrap();
    let from_software = software.sign(message).await.unwrap();
    assert_eq!(from_device.r8_x, from_software.r8_x);
    assert_eq!(from_device.r8_y, from_software.r8_y);
    assert_eq!(from_device.s, from_software.s);
}

#[tokio::test]
async fn user_denial_is_an_error_not_a_panic() {
    let mut device = MockDevice::new();
    device.deny_signing = true;
    let ledger = LedgerSigner::connect(device, ChainId::All, ACCOUNT_INDEX)
        .await
        .unwrap();

    let Err(err) = ledger.sign(U256::from(1u64)).await else {
        panic!("signing must fail");
    };
    assert!(err.to_string().contains("denied by user"), "got: {err}");
}

#[tokio::test]
async fn wrong_key_signature_is_rejected() {
    let mut device = MockDevice::new();
    device.sign_with_wrong_key = true;
    let ledger = LedgerSigner::connect(device, ChainId::All, ACCOUNT_INDEX)
        .await
        .unwrap();

    let Err(err) = ledger.sign(U256::from(1u64)).await else {
        panic!("signing must fail");
    };
    assert!(err.to_string().contains("does not verify"), "got: {err}");
}

#[tokio::test]
async fn cached_public_material_skips_the_spending_key_prompt() {
    let software = software_signer();
    let cached = Some((software.spending_public_key(), software.address()));

    let device = MockDevice::new();
    let ledger = LedgerSigner::connect_cached(device, ChainId::All, ACCOUNT_INDEX, cached)
        .await
        .unwrap();

    assert_eq!(ledger.address(), software.address());
    assert_eq!(
        ledger
            .device()
            .spending_pubkey_requests
            .load(Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn cache_from_another_account_is_refreshed_from_the_device() {
    let other = PrivateKeySigner::new(
        SpendingKey::from_hex(&hex::encode([7u8; 32])).unwrap(),
        ViewingKey::from_hex(&hex::encode([9u8; 32])).unwrap(),
        ChainId::All,
    );
    let cached = Some((other.spending_public_key(), other.address()));

    let ledger =
        LedgerSigner::connect_cached(MockDevice::new(), ChainId::All, ACCOUNT_INDEX, cached)
            .await
            .unwrap();

    assert_eq!(ledger.address(), software_signer().address());
    assert_eq!(
        ledger
            .device()
            .spending_pubkey_requests
            .load(Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn stale_handle_is_reconnected_once_and_signing_succeeds() {
    let ledger = LedgerSigner::connect(MockDevice::new(), ChainId::All, ACCOUNT_INDEX)
        .await
        .unwrap();
    ledger.device().stale_handle.store(true, Ordering::SeqCst);

    let message = U256::from(7u64);
    let signature = ledger.sign(message).await.unwrap();

    assert!(ledger.spending_public_key().verify(message, &signature));
    assert_eq!(ledger.device().reconnects.load(Ordering::SeqCst), 1);
}

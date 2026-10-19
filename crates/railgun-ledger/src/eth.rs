//! The APDU protocol of Ledger's **Ethereum** app, for the wallet's public account: shielding
//! (ERC-20 approve + shield, or native shield) and the direct transport are ordinary Ethereum
//! transactions, signed here. The Railgun app and the Ethereum app never run at the same time
//! on the device; [`current_app`] tells which one is open.
//!
//! | CLA | INS | Name | P1 | Data | Response |
//! |-----|-----|------|----|------|----------|
//! | `0xB0` | `0x01` | GET_APP_AND_VERSION (OS, any app) | 0 | empty | format(1) ‖ nameLen ‖ name ‖ versionLen ‖ version ‖ … |
//! | `0xE0` | `0x02` | GET_ETH_PUBLIC_ADDRESS | `0x00` silent / `0x01` display | path | pubLen(1, =65) ‖ pubkey(65) ‖ addrLen(1, =40) ‖ address (40 ASCII hex) |
//! | `0xE0` | `0x04` | SIGN_TRANSACTION | `0x00` first chunk / `0x80` next | path ‖ payload, chunked | v(1) ‖ r(32) ‖ s(32) |
//!
//! `path` is BIP-32: number of components (1 byte) then each component as 4 bytes big-endian,
//! hardened with `0x8000_0000`. The signed payload is the transaction's signing encoding
//! (`0x02 ‖ rlp(…)` for EIP-1559). Contract calls the app cannot decode (the Railgun shield)
//! are refused with `0x6a80` unless **Blind signing** is enabled in the app's settings.
//!
//! The returned `v` is not interpreted here: its convention differs between transaction types
//! and app versions. Callers recover the address from `(r, s)` with each parity and keep the one
//! matching the expected account, which also authenticates the signer.

use crate::{
    protocol::ProtocolError,
    transport::{Apdu, Exchange},
};

pub const CLA_ETH: u8 = 0xE0;
pub const CLA_OS: u8 = 0xB0;
pub const INS_GET_APP_AND_VERSION: u8 = 0x01;
pub const INS_GET_ADDRESS: u8 = 0x02;
pub const INS_SIGN_TRANSACTION: u8 = 0x04;
pub const P1_FIRST_CHUNK: u8 = 0x00;
pub const P1_NEXT_CHUNK: u8 = 0x80;

/// APDU payloads are at most 255 bytes.
const MAX_CHUNK: usize = 255;
const STATUS_OK: u16 = 0x9000;
const HARDENED: u32 = 0x8000_0000;

/// The Ethereum account at `index`, as Ledger Live (and Rabby or MetaMask on a Ledger Live
/// account) derive it: the index is the BIP-44 account, `m/44'/60'/index'/0/0`. A phrase's
/// accounts in MetaMask or Rabby are `m/44'/60'/0'/0/index` instead; both agree on index 0 only.
pub fn eth_path(index: u32) -> String {
    format!("m/44'/60'/{index}'/0/0")
}

/// BIP-32 path serialization: count ‖ components (4 bytes big-endian, hardened bit set).
pub fn serialize_path(path: &str) -> Result<Vec<u8>, ProtocolError> {
    let bad = || ProtocolError::Key(format!("invalid BIP-32 path {path:?}"));
    let rest = path.strip_prefix("m/").ok_or_else(bad)?;
    let mut out = vec![0u8];
    for part in rest.split('/') {
        let (num, hardened) = match part.strip_suffix('\'') {
            Some(n) => (n, true),
            None => (part, false),
        };
        let mut value: u32 = num.parse().map_err(|_| bad())?;
        if value >= HARDENED {
            return Err(bad());
        }
        if hardened {
            value |= HARDENED;
        }
        out.extend_from_slice(&value.to_be_bytes());
        out[0] += 1;
    }
    if out[0] == 0 || out[0] > 10 {
        return Err(bad());
    }
    Ok(out)
}

async fn raw<E: Exchange>(
    device: &E,
    cla: u8,
    ins: u8,
    p1: u8,
    data: Vec<u8>,
) -> Result<Vec<u8>, ProtocolError> {
    let response = device
        .exchange(&Apdu {
            cla,
            ins,
            p1,
            p2: 0,
            data,
        })
        .await?;
    if response.status != STATUS_OK {
        return Err(ProtocolError::Status {
            status: response.status,
        });
    }
    Ok(response.data)
}

/// Name and version of the app currently open on the device (`"BOLOS"` on the dashboard).
/// Answered by the OS, whichever app runs.
pub async fn current_app<E: Exchange>(device: &E) -> Result<(String, String), ProtocolError> {
    let data = raw(device, CLA_OS, INS_GET_APP_AND_VERSION, 0, Vec::new()).await?;
    let field = |at: usize| -> Option<(String, usize)> {
        let len = *data.get(at)? as usize;
        let bytes = data.get(at + 1..at + 1 + len)?;
        Some((String::from_utf8_lossy(bytes).into_owned(), at + 1 + len))
    };
    let malformed = || ProtocolError::ResponseLength {
        expected: 3,
        got: data.len(),
    };
    // data[0] is the format byte (1).
    let (name, next) = field(1).ok_or_else(malformed)?;
    let (version, _) = field(next).ok_or_else(malformed)?;
    Ok((name, version))
}

/// Dashboard name of the ZKNOX Railgun app, as [`current_app`] reports it.
pub const RAILGUN_APP_NAME: &str = "ZKNOX";
/// Dashboard name of Ledger's Ethereum app.
pub const ETHEREUM_APP_NAME: &str = "Ethereum";
/// [`current_app`]'s answer on the dashboard (no app open).
pub const DASHBOARD_NAME: &str = "BOLOS";

pub const INS_QUIT_APP: u8 = 0xA7;
pub const INS_OPEN_APP: u8 = 0xD8;
/// [`open_app`]: no installed app has this name.
pub const STATUS_APP_NOT_INSTALLED: u16 = 0x6807;

/// Closes the running app and returns to the dashboard (`CLA 0xB0, INS 0xA7`, no confirmation).
/// The device re-enumerates on USB: the answer may never arrive and the handle goes stale, so a
/// transport error here is expected; reconnect before the next command.
pub async fn quit_app<E: Exchange>(device: &E) -> Result<(), ProtocolError> {
    match raw(device, CLA_OS, INS_QUIT_APP, 0, Vec::new()).await {
        Ok(_) | Err(ProtocolError::Transport(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Launches an installed app by its dashboard name (`CLA 0xE0, INS 0xD8`). Only answered on the
/// dashboard; depending on the firmware the user confirms on the device. The device then
/// re-enumerates (a transport error is expected). `0x6807`: no such app installed.
pub async fn open_app<E: Exchange>(device: &E, name: &str) -> Result<(), ProtocolError> {
    match raw(device, CLA_ETH, INS_OPEN_APP, 0, name.as_bytes().to_vec()).await {
        Ok(_) | Err(ProtocolError::Transport(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// An Ethereum account's public key (65 bytes, uncompressed) and `0x` address, as the app
/// reports them. `display` shows the address on the device for confirmation.
pub async fn get_address<E: Exchange>(
    device: &E,
    path: &str,
    display: bool,
) -> Result<(Vec<u8>, String), ProtocolError> {
    let data = raw(
        device,
        CLA_ETH,
        INS_GET_ADDRESS,
        u8::from(display),
        serialize_path(path)?,
    )
    .await?;
    let short = || ProtocolError::ResponseLength {
        expected: 1 + 65 + 1 + 40,
        got: data.len(),
    };
    let pub_len = *data.first().ok_or_else(short)? as usize;
    let pubkey = data.get(1..1 + pub_len).ok_or_else(short)?.to_vec();
    let addr_len = *data.get(1 + pub_len).ok_or_else(short)? as usize;
    let addr = data
        .get(2 + pub_len..2 + pub_len + addr_len)
        .ok_or_else(short)?;
    let addr = String::from_utf8_lossy(addr).into_owned();
    if addr.len() != 40 || !addr.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ProtocolError::Key(format!("unexpected address {addr:?}")));
    }
    Ok((pubkey, format!("0x{addr}")))
}

/// Raw signature from SIGN_TRANSACTION.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthSignature {
    /// As returned by the app; see the module docs before using it.
    pub v: u8,
    pub r: [u8; 32],
    pub s: [u8; 32],
}

/// The error as shown to the user, with what to do when the Ethereum app refuses a contract
/// call it cannot decode.
pub fn describe(err: &ProtocolError) -> String {
    match err {
        ProtocolError::Status { status: 0x6a80 } => format!(
            "{err} (Ethereum app: enable Blind signing in its settings to sign contract calls it \
             cannot decode)"
        ),
        _ => err.to_string(),
    }
}

/// Signs a transaction's signing payload (`0x02 ‖ rlp(…)` for EIP-1559) with the account at
/// `path`. The device shows the transaction for review; contract data it cannot decode needs
/// Blind signing enabled (`0x6a80` otherwise).
pub async fn sign_transaction<E: Exchange>(
    device: &E,
    path: &str,
    payload: &[u8],
) -> Result<EthSignature, ProtocolError> {
    let mut first = serialize_path(path)?;
    let room = MAX_CHUNK - first.len();
    let (head, mut rest) = payload.split_at(payload.len().min(room));
    first.extend_from_slice(head);

    let mut response = raw(device, CLA_ETH, INS_SIGN_TRANSACTION, P1_FIRST_CHUNK, first).await?;
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(rest.len().min(MAX_CHUNK));
        response = raw(
            device,
            CLA_ETH,
            INS_SIGN_TRANSACTION,
            P1_NEXT_CHUNK,
            chunk.to_vec(),
        )
        .await?;
        rest = tail;
    }

    if response.len() != 65 {
        return Err(ProtocolError::ResponseLength {
            expected: 65,
            got: response.len(),
        });
    }
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    r.copy_from_slice(&response[1..33]);
    s.copy_from_slice(&response[33..65]);
    Ok(EthSignature {
        v: response[0],
        r,
        s,
    })
}

#[cfg(all(test, native))]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::transport::{ApduResponse, TransportError};

    /// Records the APDUs it gets and answers from a script.
    struct Scripted {
        sent: Mutex<Vec<Apdu>>,
        answers: Mutex<Vec<ApduResponse>>,
    }

    impl Scripted {
        fn new(answers: Vec<ApduResponse>) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                answers: Mutex::new(answers.into_iter().rev().collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Exchange for Scripted {
        async fn exchange(&self, apdu: &Apdu) -> Result<ApduResponse, TransportError> {
            self.sent.lock().unwrap().push(apdu.clone());
            self.answers
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| TransportError("script exhausted".into()))
        }
    }

    fn ok(data: Vec<u8>) -> ApduResponse {
        ApduResponse { data, status: 0x9000 }
    }

    #[test]
    fn path_serialization() {
        assert_eq!(
            serialize_path("m/44'/60'/0'/0/3").unwrap(),
            vec![
                5, 0x80, 0, 0, 44, 0x80, 0, 0, 60, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3
            ]
        );
        assert!(serialize_path("44'/60'").is_err());
        assert!(serialize_path("m/x").is_err());
        assert_eq!(eth_path(0), "m/44'/60'/0'/0/0");
        assert_eq!(eth_path(1), "m/44'/60'/1'/0/0");
    }

    #[tokio::test]
    async fn parses_address_and_app() {
        let mut addr = vec![65];
        addr.extend_from_slice(&[4u8; 65]);
        addr.push(40);
        addr.extend_from_slice(b"aabbccddeeff00112233445566778899aabbccdd");
        let mut app = vec![1, 8];
        app.extend_from_slice(b"Ethereum");
        app.push(6);
        app.extend_from_slice(b"1.13.1");
        let dev = Scripted::new(vec![ok(addr), ok(app)]);
        let (pubkey, address) = get_address(&dev, &eth_path(0), false).await.unwrap();
        assert_eq!(pubkey.len(), 65);
        assert_eq!(address, "0xaabbccddeeff00112233445566778899aabbccdd");
        let (name, version) = current_app(&dev).await.unwrap();
        assert_eq!((name.as_str(), version.as_str()), ("Ethereum", "1.13.1"));
        let sent = dev.sent.lock().unwrap();
        assert_eq!((sent[0].cla, sent[0].ins, sent[0].p1), (CLA_ETH, INS_GET_ADDRESS, 0));
        assert_eq!((sent[1].cla, sent[1].ins), (CLA_OS, INS_GET_APP_AND_VERSION));
    }

    #[tokio::test]
    async fn chunks_long_payloads() {
        // 600-byte payload: first APDU = 21-byte path + 234 bytes, then 255, then 111.
        let payload: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let mut sig = vec![1u8];
        sig.extend_from_slice(&[0x11; 32]);
        sig.extend_from_slice(&[0x22; 32]);
        let dev = Scripted::new(vec![ok(vec![]), ok(vec![]), ok(sig)]);
        let out = sign_transaction(&dev, &eth_path(0), &payload).await.unwrap();
        assert_eq!(out.v, 1);
        assert_eq!(out.r, [0x11; 32]);
        assert_eq!(out.s, [0x22; 32]);
        let sent = dev.sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        assert_eq!((sent[0].p1, sent[0].data.len()), (P1_FIRST_CHUNK, 255));
        assert_eq!((sent[1].p1, sent[1].data.len()), (P1_NEXT_CHUNK, 255));
        assert_eq!((sent[2].p1, sent[2].data.len()), (P1_NEXT_CHUNK, 111));
        let mut rebuilt = sent[0].data[21..].to_vec();
        rebuilt.extend_from_slice(&sent[1].data);
        rebuilt.extend_from_slice(&sent[2].data);
        assert_eq!(rebuilt, payload);
    }

    #[tokio::test]
    async fn app_switching_apdus() {
        // quit: stale handle (no answer) is fine; open: name as data, not-installed surfaced.
        let dev = Scripted::new(vec![
            ok(vec![]),
            ApduResponse { data: vec![], status: STATUS_APP_NOT_INSTALLED },
        ]);
        quit_app(&dev).await.unwrap();
        let err = open_app(&dev, ETHEREUM_APP_NAME).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Status { status: STATUS_APP_NOT_INSTALLED }));
        let sent = dev.sent.lock().unwrap();
        assert_eq!((sent[0].cla, sent[0].ins), (CLA_OS, INS_QUIT_APP));
        assert_eq!((sent[1].cla, sent[1].ins), (CLA_ETH, INS_OPEN_APP));
        assert_eq!(sent[1].data, b"Ethereum");
        drop(sent);
        // A script exhausted mid-quit is a transport error: still Ok.
        quit_app(&Scripted::new(vec![])).await.unwrap();
    }

    #[tokio::test]
    async fn surfaces_blind_signing_refusal() {
        let dev = Scripted::new(vec![ApduResponse {
            data: vec![],
            status: 0x6a80,
        }]);
        let err = sign_transaction(&dev, &eth_path(0), &[2, 0xc0])
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::Status { status: 0x6a80 }));
        assert!(describe(&err).contains("Blind signing"));
    }
}

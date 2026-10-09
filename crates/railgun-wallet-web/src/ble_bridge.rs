//! Ledger over Web Bluetooth, bridged from the page.
//!
//! Web Bluetooth only exists on the main thread (unlike WebUSB, which this worker uses
//! directly), so the page owns the GATT session (`shim.js`: device picker, notify
//! subscription, writes) and this module speaks the Ledger BLE protocol over two primitives:
//!
//! - `bleWrite(bytes) -> Promise` — a global installed by `worker.js`, forwarding one frame to
//!   the page's write characteristic and resolving on its ack;
//! - [`ble_notify`] — called by `worker.js` for every notification frame the page forwards.
//!
//! All framing (MTU handshake, `0x05` chunking, reassembly) stays in Rust, reusing the
//! unit-tested `railgun_ledger::transport::ble_framing` — the page never parses a frame.

use std::cell::RefCell;

use futures::{
    FutureExt, StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
    lock::Mutex,
};
use railgun_ledger::transport::{
    Apdu, ApduResponse, Exchange, TransportError,
    ble_framing::{self, DEFAULT_MTU},
};
use wasm_bindgen::{JsCast, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;

thread_local! {
    static NOTIFY_TX: RefCell<Option<UnboundedSender<Vec<u8>>>> = const { RefCell::new(None) };
}

/// Entry point for `worker.js`: one call per BLE notification frame forwarded by the page.
/// Frames arriving while no bridged device is connecting/connected are dropped.
#[wasm_bindgen(js_name = "bleNotify")]
pub fn ble_notify(bytes: &[u8]) {
    NOTIFY_TX.with(|tx| {
        if let Some(tx) = tx.borrow().as_ref() {
            let _ = tx.unbounded_send(bytes.to_vec());
        }
    });
}

fn err(context: &str, e: impl std::fmt::Debug) -> TransportError {
    TransportError(format!("{context}: {e:?}"))
}

/// One frame to the page's write characteristic, awaiting its ack.
async fn ble_write(frame: &[u8]) -> Result<(), TransportError> {
    let global = js_sys::global();
    let f: js_sys::Function = js_sys::Reflect::get(&global, &"bleWrite".into())
        .ok()
        .and_then(|v| v.dyn_into().ok())
        .ok_or_else(|| TransportError("no bleWrite bridge in this worker".into()))?;
    let arr = js_sys::Uint8Array::from(frame);
    let promise: js_sys::Promise = f
        .call1(&global, &arr)
        .map_err(|e| err("bleWrite call", e))?
        .dyn_into()
        .map_err(|e| err("bleWrite did not return a promise", e))?;
    JsFuture::from(promise)
        .await
        .map_err(|e| err("bleWrite", e))?;
    Ok(())
}

/// A Ledger reached through the page's Web Bluetooth session.
pub struct BridgedBleLedger {
    mtu: usize,
    rx: Mutex<UnboundedReceiver<Vec<u8>>>,
}

impl BridgedBleLedger {
    /// The page must have connected the device already (shim.js does it during the unlock
    /// click, whose gesture the device picker needs). Performs the MTU handshake.
    pub async fn connect() -> Result<Self, TransportError> {
        let (tx, mut rx) = unbounded::<Vec<u8>>();
        NOTIFY_TX.with(|t| *t.borrow_mut() = Some(tx));

        // MTU handshake: write 0x08, read the 0x08 reply. Bounded: a silent page (device
        // connected but notifications not flowing) must fail, not hang the unlock.
        ble_write(&ble_framing::mtu_request()).await?;
        let mut mtu = DEFAULT_MTU;
        for _ in 0..8 {
            let frame = futures::select! {
                f = rx.next() => f,
                _ = gloo_timers::future::TimeoutFuture::new(5_000).fuse() => {
                    return Err(TransportError(
                        "no BLE frame from the page within 5s (device connected and app open?)"
                            .into(),
                    ));
                }
            };
            match frame {
                Some(frame) => {
                    if let Some(m) = ble_framing::parse_mtu(&frame) {
                        mtu = m;
                        break;
                    }
                }
                None => break,
            }
        }

        Ok(Self { mtu, rx: Mutex::new(rx) })
    }
}

#[async_trait::async_trait(?Send)]
impl Exchange for BridgedBleLedger {
    async fn exchange(&self, apdu: &Apdu) -> Result<ApduResponse, TransportError> {
        let mut raw = Vec::with_capacity(5 + apdu.data.len());
        raw.extend_from_slice(&[apdu.cla, apdu.ins, apdu.p1, apdu.p2, apdu.data.len() as u8]);
        raw.extend_from_slice(&apdu.data);

        let mut rx = self.rx.lock().await;
        // Discard stale/echo frames buffered before this request.
        while rx.next().now_or_never().flatten().is_some() {}

        for frame in ble_framing::frames(self.mtu, &raw) {
            ble_write(&frame).await?;
        }

        // No timeout: a blind-sign approval legitimately waits on the user.
        let mut re = ble_framing::Reassembler::new();
        let response = loop {
            let frame = rx
                .next()
                .await
                .ok_or_else(|| TransportError("BLE notification channel closed".into()))?;
            if let Some(r) = re.feed(&frame).map_err(TransportError)? {
                break r;
            }
        };

        if response.len() < 2 {
            return Err(TransportError("BLE response shorter than status word".into()));
        }
        let split = response.len() - 2;
        let status = u16::from_be_bytes([response[split], response[split + 1]]);
        Ok(ApduResponse { data: response[..split].to_vec(), status })
    }
}

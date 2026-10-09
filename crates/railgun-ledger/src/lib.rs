//! Ledger support for the ZKNOX Railgun wallet.
//!
//! The Railgun signer itself (APDU protocol of the ZKNOX Railgun app, [`LedgerSigner`]) is in
//! the Kohaku SDK, `kohaku-railgun-ledger`, re-exported here. This crate adds what the SDK
//! leaves to applications:
//!
//! - [`transport`] — [`Exchange`] over USB HID (feature `usb`, native), Bluetooth LE (`ble`,
//!   native), WebUSB (`webusb`, wasm) and Web Bluetooth (`webble`, wasm).
//! - [`eth`] — the Ethereum app, which signs the wallet's public-account transactions
//!   (shield), and switching between the two apps.

pub mod eth;
pub mod transport;

pub use kohaku_railgun_ledger::{LedgerError, LedgerSigner, protocol, signer};
pub use transport::{Apdu, ApduResponse, Exchange, TransportError};

#[cfg(all(native, feature = "ble"))]
pub use transport::ble::BleLedger;

#[cfg(all(wasm, feature = "webusb"))]
pub use transport::webusb::WebUsbLedger;

#[cfg(all(wasm, feature = "webble"))]
pub use transport::webble::WebBleLedger;

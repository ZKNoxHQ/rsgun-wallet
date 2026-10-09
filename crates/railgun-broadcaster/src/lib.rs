//! Client for the Railgun community broadcaster network.
//!
//! A broadcaster is a relayer that submits a shielded transaction from its own EOA and is paid by
//! a note inside that transaction. Wallet and broadcasters talk over Waku:
//!
//! 1. broadcasters announce signed fee rates on the fees topic ([`fees`]);
//! 2. the wallet builds the transaction with the fee note first and `minGasPrice` bound, proves
//!    it together with the pre-transaction POIs (`railgun` crate);
//! 3. the request is sealed for the chosen broadcaster and published ([`client`]);
//! 4. the answer, a transaction hash or an error, comes back sealed with the same key.
//!
//! Protocol details follow `@railgun-community/waku-broadcaster-client` 9.x.

pub mod client;
pub mod crypto;
pub mod fees;
pub mod time;
#[cfg(any(test, feature = "testing"))]
pub mod mock;
pub mod transport;
pub mod wire;

pub use client::{BroadcastRequest, BroadcasterClient, ClientError, SealedRequest};
pub use fees::{
    FeeQuote, NoQuote, PAR_RATE_WRAPPED_BASE_TOKEN, RAILWAY_TRUSTED_FEE_SIGNERS, TrustPolicy,
    token_fee,
};
pub use transport::{
    WakuTransport,
    bridge::{BrowserBridge, Outbound, PublishAck, PublishStats, RemoteStatus},
    nwaku_rest::NwakuRest,
};
#[cfg(feature = "light-node")]
pub use transport::light::LightNodeTransport;

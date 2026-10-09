//! `BroadcasterClient` on the Rust Waku light node, in a browser: fee announcements of the
//! Railgun fleet authenticated, capped by the trusted signers and ranked, as the wallet does.
//! Passive: it publishes nothing.
//!
//! Build and serve: `crates/railgun-broadcaster/examples/web/build.sh`, then
//! http://localhost:8089. On native targets this example compiles to an empty library.
#![cfg(target_arch = "wasm32")]

use std::{cell::Cell, rc::Rc, sync::{Arc, Once}};

use js_sys::Promise;
use railgun_broadcaster::{BroadcasterClient, LightNodeTransport, RAILWAY_TRUSTED_FEE_SIGNERS};
use serde_json::json;
use wasm_bindgen::prelude::*;

fn init_once() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        console_error_panic_hook::set_once();
        tracing_wasm::set_as_global_default_with_config(
            tracing_wasm::WASMLayerConfigBuilder::new()
                .set_max_level(tracing::Level::INFO)
                .build(),
        );
    });
}

#[wasm_bindgen]
pub struct WebBroadcasters {
    client: Rc<BroadcasterClient>,
    transport: Arc<LightNodeTransport>,
    subscribed: Rc<Cell<bool>>,
}

#[wasm_bindgen]
impl WebBroadcasters {
    /// `trusted`: cap offers with the Railway trusted fee signers, as the wallet does by default.
    #[wasm_bindgen(constructor)]
    pub fn new(chain_id: u32, trusted: bool) -> Result<WebBroadcasters, JsError> {
        init_once();
        let chain = u64::from(chain_id);
        let transport = Arc::new(LightNodeTransport::for_chain(chain));
        let client = if trusted {
            let signers: Vec<String> = RAILWAY_TRUSTED_FEE_SIGNERS.iter().map(|s| s.to_string()).collect();
            BroadcasterClient::with_trusted_signers(transport.clone(), chain, &signers)
                .map_err(|e| JsError::new(&e.to_string()))?
        } else {
            BroadcasterClient::new(transport.clone(), chain)
        };
        Ok(WebBroadcasters { client: Rc::new(client), transport, subscribed: Rc::new(Cell::new(false)) })
    }

    /// One round of the wallet's fee monitor: subscribe until it holds, then drain. Resolves to
    /// the number of announcements accepted; rejects with the transport's message (not ready…).
    pub fn tick(&self) -> Promise {
        let client = self.client.clone();
        let subscribed = self.subscribed.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            if !subscribed.get() {
                client.subscribe().await.map_err(|e| JsValue::from_str(&e.to_string()))?;
                subscribed.set(true);
            }
            match client.pump().await {
                Ok(n) => Ok(JsValue::from(n as u32)),
                Err(e) => {
                    subscribed.set(false);
                    Err(JsValue::from_str(&e.to_string()))
                }
            }
        })
    }

    /// Live offers, JSON (`FeeQuote` list, camelCase, rates as decimal strings).
    pub fn quotes(&self) -> Result<String, JsError> {
        serde_json::to_string(&self.client.all_quotes()).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Rates announced by the trusted signers, JSON `[{ token, feePerUnitGas }]`.
    #[wasm_bindgen(js_name = authorizedFees)]
    pub fn authorized_fees(&self) -> String {
        let list: Vec<_> = self
            .client
            .authorized_fees()
            .into_iter()
            .map(|(token, rate)| json!({ "token": token, "feePerUnitGas": rate.to_string() }))
            .collect();
        serde_json::Value::Array(list).to_string()
    }

    #[wasm_bindgen(js_name = trustedSigners)]
    pub fn trusted_signers(&self) -> u32 {
        self.client.trusted_signer_count() as u32
    }

    /// Node state, JSON `{ connectedPeers, servicePeers, filterSubscriptions, lastError }`, or
    /// `null` before the first tick started the node.
    pub fn status(&self) -> String {
        match self.transport.status() {
            Some(s) => json!({
                "connectedPeers": s.connected_peers,
                "servicePeers": s.service_peers,
                "filterSubscriptions": s.filter_subscriptions,
                "lastError": s.last_error,
            })
            .to_string(),
            None => "null".into(),
        }
    }
}

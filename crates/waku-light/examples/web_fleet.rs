//! waku-light in a browser: a light node on the page's own WebSocket, listening to the fee
//! announcements of the Railgun fleet. The only thing it can publish is the echo test, on a
//! content topic of its own that no broadcaster listens to.
//!
//! Build and serve: `crates/waku-light/examples/web/build.sh`, then open http://localhost:8088.
//! On native targets this example compiles to an empty library.
#![cfg(target_arch = "wasm32")]

use std::{rc::Rc, sync::Once};

use js_sys::{Array, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use waku_light::{Config, LightNode};

/// wss peers of the Railgun fleet, as pinned by the reference web client (identities have since
/// rotated; the node accepts the new ones over wss).
const FLEET: [&str; 3] = [
    "/dns4/relay-a.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmFbD2ZvAFi2j9jjDo6g4HFbQAhfjDfnTTrbyRGQRmtG7x",
    "/dns4/relay-b.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmPtEAoPPok7VLrpNNC6t92ZQFqLndHvkdx6Fk3CxA4MaG",
    "/dns4/client-edge.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmQdCGG5qREQCq96kucmpUVupmvLwrTRjMazPAaMTNP97A",
];

fn set(obj: &Object, key: &str, value: impl Into<JsValue>) {
    let _ = Reflect::set(obj, &JsValue::from_str(key), &value.into());
}

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

/// One light node, subscribed to the fees topic of a chain. `free()` stops it.
#[wasm_bindgen]
pub struct WebNode {
    node: Rc<LightNode>,
    topic: String,
}

#[wasm_bindgen]
impl WebNode {
    #[wasm_bindgen(constructor)]
    pub fn new(chain_id: u32) -> Result<WebNode, JsError> {
        init_once();
        let bootstrap = FLEET
            .iter()
            .map(|a| a.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| JsError::new(&format!("bad fleet address: {e}")))?;
        let node = LightNode::start(Config::new(bootstrap, 5, 1)).map_err(|e| JsError::new(&e.to_string()))?;
        let topic = format!("/railgun/v2/0-{chain_id}-fees/json");
        node.subscribe([topic.clone()]);
        Ok(WebNode { node: Rc::new(node), topic })
    }

    #[wasm_bindgen(js_name = peerId)]
    pub fn peer_id(&self) -> String {
        self.node.local_peer_id().to_string()
    }

    pub fn topic(&self) -> String {
        self.topic.clone()
    }

    /// Adds a content topic to the filter subscription.
    pub fn subscribe(&self, topic: String) {
        self.node.subscribe([topic]);
    }

    /// Light push. Resolves to `{ accepted, via: [..], failures: [..] }`, rejects when no peer
    /// accepted.
    pub fn publish(&self, topic: String, payload: Vec<u8>) -> Promise {
        let node = self.node.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            match node.publish(&topic, &payload).await {
                Ok(report) => {
                    let o = Object::new();
                    set(&o, "accepted", report.accepted as u32);
                    set(&o, "via", report.accepted_via.iter().map(|s| JsValue::from_str(s)).collect::<Array>());
                    set(&o, "failures", report.failures.iter().map(|s| JsValue::from_str(s)).collect::<Array>());
                    Ok(o.into())
                }
                Err(e) => Err(JsValue::from_str(&e.to_string())),
            }
        })
    }

    /// `{ connectedPeers, servicePeers, filterSubscriptions, lastError }`
    pub fn status(&self) -> Object {
        let s = self.node.status();
        let o = Object::new();
        set(&o, "connectedPeers", s.connected_peers as u32);
        set(&o, "servicePeers", s.service_peers as u32);
        set(&o, "filterSubscriptions", s.filter_subscriptions as u32);
        set(&o, "lastError", s.last_error.map_or(JsValue::NULL, |e| JsValue::from_str(&e)));
        o
    }

    /// Messages received since the previous call: `[{ contentTopic, payload: Uint8Array,
    /// timestampMs }]`.
    pub fn drain(&self) -> Array {
        let out = Array::new();
        for m in self.node.drain() {
            let o = Object::new();
            set(&o, "contentTopic", m.content_topic.as_str());
            set(&o, "payload", Uint8Array::from(m.payload.as_slice()));
            set(&o, "timestampMs", m.timestamp_ns.map_or(JsValue::NULL, |t| JsValue::from_f64((t / 1_000_000) as f64)));
            out.push(&o);
        }
        out
    }
}

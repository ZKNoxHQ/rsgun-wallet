//! Key-value storage over a JavaScript database object.
//!
//! The object implements the `Database` interface below; the web wallets back it with IndexedDB
//! (`js/idb.js`). Keys are strings. Values are `Uint8Array`s, or hex strings for entries written
//! by earlier versions, which read back the same.
//!
//! JS futures are not `Send`. wasm32 is single-threaded, so each future is wrapped in a
//! [`SendWrapper`], which only asserts that it is polled on the thread that created it.

use std::fmt;

use js_sys::Uint8Array;
use send_wrapper::SendWrapper;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};

#[wasm_bindgen(typescript_custom_section)]
const TS_INTERFACE: &str = r#"
/**
 * Key-value database for the wallets' persisted state. Keys are strings, values are
 * Uint8Arrays (hex strings from earlier versions are read back as bytes).
 */
export interface Database {
    get(key: string): Promise<Uint8Array | string | null>;
    set(key: string, value: Uint8Array): Promise<void>;
    delete(key: string): Promise<void>;
}
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type = "Database")]
    pub type JsDatabase;

    #[wasm_bindgen(method, catch, js_name = "get")]
    async fn get(this: &JsDatabase, key: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "set")]
    async fn set(this: &JsDatabase, key: &str, value: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch, js_name = "delete")]
    async fn delete(this: &JsDatabase, key: &str) -> Result<(), JsValue>;
}

/// Byte values over a [`JsDatabase`].
pub struct JsKvStore(SendWrapper<JsDatabase>);

#[derive(Debug)]
pub struct JsError(String);

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsError {}

fn js_error(op: &str, e: impl fmt::Debug) -> JsError {
    JsError(format!("JS {op} error: {e:?}"))
}

impl JsKvStore {
    pub fn new(db: JsDatabase) -> Self {
        Self(SendWrapper::new(db))
    }

    /// The bytes stored under `key`, if any.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, JsError> {
        let key = key.to_string();
        SendWrapper::new(async move {
            let value = self.0.get(&key).await.map_err(|e| js_error("get", e))?;
            if value.is_null() || value.is_undefined() {
                return Ok(None);
            }
            if let Some(hex) = value.as_string() {
                return Ok(Some(hex::decode(hex).map_err(|e| js_error("get", e))?));
            }
            let bytes = value
                .dyn_into::<Uint8Array>()
                .map_err(|_| js_error("get", "value is neither a Uint8Array nor a string"))?;
            Ok(Some(bytes.to_vec()))
        })
        .await
    }

    /// Stores `value` under `key`, replacing any previous value atomically.
    pub async fn put(&self, key: &str, value: &[u8]) -> Result<(), JsError> {
        let key = key.to_string();
        let value = SendWrapper::new(JsValue::from(Uint8Array::from(value)));
        SendWrapper::new(async move {
            self.0
                .set(&key, &value)
                .await
                .map_err(|e| js_error("set", e))
        })
        .await
    }

    pub async fn delete(&self, key: &str) -> Result<(), JsError> {
        let key = key.to_string();
        SendWrapper::new(async move { self.0.delete(&key).await.map_err(|e| js_error("delete", e)) })
            .await
    }
}

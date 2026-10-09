//! A [`KvStoreBackend`] over a JavaScript database object.
//!
//! The object implements the `Database` interface below; the web wallets back it with IndexedDB
//! (`js/idb.js`). Keys and values cross the boundary hex-encoded.
//!
//! JS futures are not `Send`, while [`KvStoreBackend`] requires `Send` futures. wasm32 is
//! single-threaded, so each future is wrapped in a [`SendWrapper`], which only asserts that it
//! is polled on the thread that created it.

use std::fmt;

use kohaku_kv_store::{Store, backend::KvStoreBackend, backend::StoreError};
use send_wrapper::SendWrapper;
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

#[wasm_bindgen(typescript_custom_section)]
const TS_INTERFACE: &str = r#"
/**
 * Key-value database for the railgun provider state. Keys and values are hex strings.
 */
export interface Database {
    get(key: string): Promise<string | null>;
    set(key: string, value: string): Promise<void>;
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
    async fn set(this: &JsDatabase, key: &str, value: &str) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch, js_name = "delete")]
    async fn delete(this: &JsDatabase, key: &str) -> Result<(), JsValue>;
}

/// [`KvStoreBackend`] over a [`JsDatabase`].
pub struct JsKvStore(SendWrapper<JsDatabase>);

impl JsKvStore {
    pub fn new(db: JsDatabase) -> Self {
        Self(SendWrapper::new(db))
    }

    /// A [`Store`] over `db`.
    pub fn store(db: JsDatabase) -> Store {
        Store::new(Self::new(db))
    }
}

#[derive(Debug)]
struct JsError(String);

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsError {}

fn js_error(op: &str, e: impl fmt::Debug) -> StoreError {
    StoreError::from(Box::new(JsError(format!("JS {op} error: {e:?}")))
        as Box<dyn std::error::Error + Send + Sync>)
}

#[async_trait::async_trait]
impl KvStoreBackend for JsKvStore {
    async fn get_batch(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let keys: Vec<String> = keys.iter().map(hex::encode).collect();
        SendWrapper::new(async {
            let mut out = Vec::with_capacity(keys.len());
            for key in &keys {
                let value = self.0.get(key).await.map_err(|e| js_error("get", e))?;
                if value.is_null() || value.is_undefined() {
                    out.push(None);
                    continue;
                }
                let value = value
                    .as_string()
                    .ok_or_else(|| js_error("get", "value is not a string"))?;
                out.push(Some(hex::decode(value).map_err(|e| js_error("get", e))?));
            }
            Ok(out)
        })
        .await
    }

    /// Writes in order, one key at a time: callers that save dependent keys put the data before
    /// the key that points to it, so an interrupted batch never references unsaved data.
    async fn put_batch(&self, items: &[(&[u8], &[u8])]) -> Result<(), StoreError> {
        let items: Vec<(String, String)> = items
            .iter()
            .map(|(k, v)| (hex::encode(k), hex::encode(v)))
            .collect();
        SendWrapper::new(async {
            for (key, value) in &items {
                self.0.set(key, value).await.map_err(|e| js_error("set", e))?;
            }
            Ok(())
        })
        .await
    }

    async fn delete_batch(&self, keys: &[&[u8]]) -> Result<(), StoreError> {
        let keys: Vec<String> = keys.iter().map(hex::encode).collect();
        SendWrapper::new(async {
            for key in &keys {
                self.0.delete(key).await.map_err(|e| js_error("delete", e))?;
            }
            Ok(())
        })
        .await
    }
}

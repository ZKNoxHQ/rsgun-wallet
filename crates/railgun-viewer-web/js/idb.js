// IndexedDB storage for the web build, imported by the wasm module (wasm-bindgen snippet).
// One IndexedDB database per account and chain ("drrail-<chain>-<tag>"), one object store "kv":
// the SDK state snapshot under "railgun-state" (a Uint8Array, stored as is: IndexedDB keeps
// typed arrays through structured cloning, nothing here turns values into strings) and the
// viewer cache under CACHE_KEY (a JSON string). Databases written by earlier versions also hold
// the SDK's old per-key entries (hex strings), which are no longer read.

const CACHE_KEY = "__viewer_cache__";
const handles = new Map();

function openIdb(name) {
  if (!handles.has(name)) {
    handles.set(name, new Promise((resolve, reject) => {
      const req = indexedDB.open(name, 1);
      req.onupgradeneeded = () => req.result.createObjectStore("kv");
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => { handles.delete(name); reject(req.error); };
    }));
  }
  return handles.get(name);
}

async function run(name, mode, op) {
  const db = await openIdb(name);
  return new Promise((resolve, reject) => {
    const tx = db.transaction("kv", mode);
    const req = op(tx.objectStore("kv"));
    let out;
    req.onsuccess = () => { out = req.result; };
    tx.oncomplete = () => resolve(out);
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error || new Error("IndexedDB transaction aborted"));
  });
}

// `kv_store_js::JsDatabase` interface (crates/kv-store-js): get returns the stored value or null.
export function openDb(name) {
  return {
    async get(key) {
      const v = await run(name, "readonly", (s) => s.get(key));
      return v === undefined ? null : v;
    },
    async set(key, value) { await run(name, "readwrite", (s) => s.put(value, key)); },
    async delete(key) { await run(name, "readwrite", (s) => s.delete(key)); },
  };
}

export async function cacheLoad(name) {
  const v = await run(name, "readonly", (s) => s.get(CACHE_KEY));
  return v === undefined ? null : v;
}

export function cacheSave(name, json) {
  return run(name, "readwrite", (s) => s.put(json, CACHE_KEY)).catch((e) => {
    console.warn("Dr Rail: cannot write the viewer cache", e);
  });
}

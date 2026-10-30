// IndexedDB storage for the web build, imported by the wasm module (wasm-bindgen snippet).
// One IndexedDB database per account and chain ("rgw-<chain>-<tag>"), one object store "kv",
// as `kv_store_js::JsDatabase` expects: string keys, `Uint8Array` values. The wallet keeps the
// SDK state snapshot under a single key; the Ledger key cache ("rgw-ledger") has one entry per
// account. Entries written by earlier versions hold hex strings, which the Rust side still reads.
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

// Deletes one wallet database (the daemon's "empty cache"). The cached connection is closed first,
// otherwise the deletion stays blocked on it.
export async function deleteDb(name) {
  if (handles.has(name)) {
    try { (await handles.get(name)).close(); } catch { /* already closed or never opened */ }
    handles.delete(name);
  }
  await new Promise((resolve, reject) => {
    const req = indexedDB.deleteDatabase(name);
    req.onsuccess = () => resolve();
    req.onerror = () => reject(req.error);
    req.onblocked = () => reject(new Error("the database is still open elsewhere: close other tabs of this wallet"));
  });
}

// `kv_store_js::JsDatabase` interface.
export function openDb(name) {
  return {
    async get(key) {
      const v = await run(name, "readonly", (s) => s.get(key));
      return v === undefined ? null : v;
    },
    // The value is stored as given: IndexedDB structured-clones a `Uint8Array`, so the bytes
    // come back as a `Uint8Array` too. Nothing here turns it into a string.
    async set(key, value) { await run(name, "readwrite", (s) => s.put(value, key)); },
    async delete(key) { await run(name, "readwrite", (s) => s.delete(key)); },
  };
}

// IndexedDB storage for the web build, imported by the wasm module (wasm-bindgen snippet).
// One IndexedDB database per account and chain ("rgw-<chain>-<tag>"), one object store "kv":
// the SDK's keys (hex strings) as `kohaku_db::js::JsDatabase` expects. Values are strings.
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

// `kohaku_db::js::JsDatabase` interface.
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

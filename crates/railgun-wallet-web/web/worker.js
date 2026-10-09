// Railgun wallet web: the wasm engine runs here, off the page's main thread (scans, proofs and
// note decryption take a while). One message per `/api/…` request forwarded by shim.js, plus the
// Ledger-BLE bridge: Web Bluetooth only exists on the main thread, so the page owns the GATT
// session and this worker exchanges raw frames with it ("ble-write" out, "ble-notify" in). All
// Ledger framing happens in the wasm.
import init, { api, bleNotify } from "./pkg/railgun_wallet_web.js";

const ready = init();

// Installed as a global for the wasm: one frame to the page's write characteristic.
const bleAcks = new Map();
let bleSeq = 0;
self.bleWrite = (bytes) =>
  new Promise((resolve, reject) => {
    const wid = ++bleSeq;
    bleAcks.set(wid, { resolve, reject });
    self.postMessage({ type: "ble-write", wid, bytes: Array.from(bytes) });
  });

self.onmessage = async (event) => {
  const d = event.data;
  if (d.type === "ble-notify") {
    await ready;
    bleNotify(new Uint8Array(d.bytes));
    return;
  }
  if (d.type === "ble-write-ack") {
    const p = bleAcks.get(d.wid);
    if (p) {
      bleAcks.delete(d.wid);
      d.error ? p.reject(new Error(d.error)) : p.resolve();
    }
    return;
  }
  const { id, method, path, body } = d;
  try {
    await ready;
    const out = await api(method, path, body || "");
    self.postMessage({ id, ok: true, body: out });
  } catch (err) {
    self.postMessage({ id, ok: false, error: String((err && err.message) || err) });
  }
};

// Railgun wallet web: the daemon's front talks to `/api/…` as with the local daemon; these calls
// are answered by the wasm engine in worker.js instead of the network. Everything else (RPC,
// subsquid, POI node, bundler, Waku) is fetched directly by the worker or the page.
(function () {
  const worker = new Worker(new URL("./worker.js", document.baseURI), { type: "module" });
  const pending = new Map();
  let seq = 0;

  worker.onmessage = (event) => {
    const d = event.data;
    if (d.type === "ble-write") {
      (async () => {
        if (!bleSession) throw new Error("no BLE device connected");
        const buf = new Uint8Array(d.bytes);
        if (bleSession.writeChar.writeValueWithResponse) await bleSession.writeChar.writeValueWithResponse(buf);
        else await bleSession.writeChar.writeValue(buf);
      })().then(
        () => worker.postMessage({ type: "ble-write-ack", wid: d.wid }),
        (e) => worker.postMessage({ type: "ble-write-ack", wid: d.wid, error: String((e && e.message) || e) }),
      );
      return;
    }
    const { id, ok, body, error } = d;
    const p = pending.get(id);
    if (!p) return;
    pending.delete(id);
    if (ok) p.resolve(body);
    else p.reject(new Error(error));
  };
  worker.onerror = (event) => {
    const msg = "engine worker failed: " + (event.message || "see the console");
    console.error("Railgun wallet:", msg, event);
    for (const p of pending.values()) p.reject(new Error(msg));
    pending.clear();
  };

  // A Ledger unlock: device pickers only exist on the main thread and need the click's user
  // gesture. USB: grant access here, the worker then opens the device with getDevices(). BLE:
  // Web Bluetooth does not exist in workers at all, so the page owns the whole GATT session and
  // bridges raw frames to the worker ("ble-notify" in, "ble-write" out); the Ledger framing
  // lives in the wasm.
  const LEDGER_VENDOR_ID = 0x2c97;
  // (service, notify characteristic, write characteristic) per device model.
  const BLE_MODELS = [
    ["13d63400-2c97-3004-0000-4c6564676572", "13d63400-2c97-3004-0001-4c6564676572", "13d63400-2c97-3004-0002-4c6564676572"], // Flex
    ["13d63400-2c97-6004-0000-4c6564676572", "13d63400-2c97-6004-0001-4c6564676572", "13d63400-2c97-6004-0002-4c6564676572"], // Stax
    ["13d63400-2c97-0004-0000-4c6564676572", "13d63400-2c97-0004-0001-4c6564676572", "13d63400-2c97-0004-0002-4c6564676572"], // Nano X
  ];
  let bleSession = null; // { device, writeChar }

  async function connectBle() {
    if (bleSession && bleSession.device.gatt && bleSession.device.gatt.connected) return;
    bleSession = null;
    if (!navigator.bluetooth) {
      throw new Error("Web Bluetooth is not available in this browser (Chromium on Android/macOS/Windows; on Linux enable chrome://flags/#enable-web-bluetooth)");
    }
    const device = await navigator.bluetooth
      .requestDevice({ filters: BLE_MODELS.map((m) => ({ services: [m[0]] })) })
      .catch((e) => { throw new Error("Ledger BLE access: " + (e && e.message ? e.message : e)); });
    const server = await device.gatt.connect();
    let chars = null;
    for (const [svc, notify, write] of BLE_MODELS) {
      try {
        const service = await server.getPrimaryService(svc);
        chars = [await service.getCharacteristic(notify), await service.getCharacteristic(write)];
        break;
      } catch { /* not this model */ }
    }
    if (!chars) { device.gatt.disconnect(); throw new Error("no Ledger GATT service on the device"); }
    const [notifyChar, writeChar] = chars;
    notifyChar.addEventListener("characteristicvaluechanged", (e) => {
      const v = e.target.value;
      worker.postMessage({ type: "ble-notify", bytes: Array.from(new Uint8Array(v.buffer, v.byteOffset, v.byteLength)) });
    });
    await notifyChar.startNotifications();
    device.addEventListener("gattserverdisconnected", () => { bleSession = null; });
    bleSession = { device, writeChar };
  }

  async function grantLedger(body) {
    let params;
    try { params = JSON.parse(body || "{}"); } catch { return; }
    if (!params.ledger) return;
    if ((params.ledgerTransport || "usb") === "ble") return connectBle();
    if (!navigator.usb) throw new Error("WebUSB is not available in this browser (use Chromium)");
    const devices = await navigator.usb.getDevices();
    if (devices.some((d) => d.vendorId === LEDGER_VENDOR_ID)) return;
    await navigator.usb.requestDevice({ filters: [{ vendorId: LEDGER_VENDOR_ID }] })
      .catch((e) => { throw new Error("Ledger USB access: " + (e && e.message ? e.message : e)); });
  }

  const realFetch = window.fetch.bind(window);
  window.fetch = function (input, init) {
    const raw = typeof input === "string" ? input : input && input.url;
    const url = new URL(raw, location.href);
    const at = url.pathname.indexOf("/api/");
    if (url.origin !== location.origin || at < 0) return realFetch(input, init);
    const path = url.pathname.slice(at) + url.search;
    const method = ((init && init.method) || "GET").toUpperCase();
    const body = init && typeof init.body === "string" ? init.body : "";
    const gate = path.startsWith("/api/unlock") && method === "POST" ? grantLedger(body) : Promise.resolve();
    const id = ++seq;
    return gate.then(() => new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      worker.postMessage({ id, method, path, body });
    })).then((text) => new Response(text, { status: 200, headers: { "content-type": "application/json" } }));
  };
})();

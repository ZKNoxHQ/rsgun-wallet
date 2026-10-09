// Dr Rail web: the viewer's front talks to `/api/…` as with the local daemon; these calls are
// answered by the wasm engine in worker.js instead of the network. Everything else (RPC, subsquid,
// POI node, Waku) is fetched directly by the worker or the page.
(function () {
  const worker = new Worker(new URL("./worker.js", document.baseURI), { type: "module" });
  const pending = new Map();
  let seq = 0;

  worker.onmessage = (event) => {
    const { id, ok, body, error } = event.data;
    const p = pending.get(id);
    if (!p) return;
    pending.delete(id);
    if (ok) p.resolve(body);
    else p.reject(new Error(error));
  };
  worker.onerror = (event) => {
    const msg = "engine worker failed: " + (event.message || "see the console");
    console.error("Dr Rail:", msg, event);
    for (const p of pending.values()) p.reject(new Error(msg));
    pending.clear();
  };

  const realFetch = window.fetch.bind(window);
  window.fetch = function (input, init) {
    const raw = typeof input === "string" ? input : input && input.url;
    const url = new URL(raw, location.href);
    const at = url.pathname.indexOf("/api/");
    if (url.origin !== location.origin || at < 0) return realFetch(input, init);
    const path = url.pathname.slice(at) + url.search;
    const method = ((init && init.method) || "GET").toUpperCase();
    const body = init && typeof init.body === "string" ? init.body : "";
    const id = ++seq;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      worker.postMessage({ id, method, path, body });
    }).then((text) => new Response(text, { status: 200, headers: { "content-type": "application/json" } }));
  };
})();

# railgun-viewer-web (Dr Rail, static site)

Dr Rail without a daemon: the session core of `zknox/dr-rail` compiled to wasm, run in a Web
Worker, behind the viewer's own front. Nothing to host but files.

## Build

    rustup target add wasm32-unknown-unknown
    cargo install wasm-bindgen-cli --version 0.2.108 --locked
    sh crates/railgun-viewer-web/build.sh

Output in `crates/railgun-viewer-web/dist/`. Local test (module workers do not run from `file://`):

    python3 -m http.server 8791 -d crates/railgun-viewer-web/dist

then http://127.0.0.1:8791. CI: `.github/workflows/dr-rail-web.yml` builds on every push to
`zknox/dr-rail` and publishes on GitHub Pages; the `dr-rail-web` artifact holds the same
files for any other static host.

## Layout

- `src/lib.rs`: the viewer's modules included by `#[path]`, plus the wallet's `keys.rs`.
- `src/web.rs`: engine actor and `/api/…` routes for the browser, IndexedDB database and cache.
- `js/idb.js`: IndexedDB store (wasm-bindgen snippet), `kohaku_db::js::JsDatabase` interface.
- `web/worker.js`, `web/shim.js`: the worker hosting the wasm, the `fetch` router of the page.
- `web/make_index.py`: the static `index.html` from `crates/railgun-viewer/static/index.html`.

## Limits

- Every service is called from the browser and must allow it (CORS): RPC, subsquid, POI node.
- Mnemonic input stays Sepolia only (front rule); use the viewing key and the 0zk address elsewhere.
- The nwaku REST field of the Network tab cannot reach a local node from an https page.

# rsgun-wallet

A Railgun wallet written in Rust, by ZKNOX, built on the [Kohaku](https://github.com/ethereum/kohaku-rs) Rust SDK.

- **Wallet:** shield, private transfer and unshield, with Private Proofs of Innocence (POI).
  Transactions go through an ERC-4337 bundler with the Railgun privacy paymaster, or through
  the community broadcasters over Waku.
- **Signers:** software keys (mnemonic or raw keys), or a **Ledger** running the ZKNOX Railgun
  app. With a Ledger, the spending key never leaves the device and each transaction is signed
  once. The public account (shielding) can be signed by the Ledger's Ethereum app.
- **Two forms:** a local daemon with a web front, and a fully static web page (the engine
  compiled to wasm, running in a Web Worker), deployable on any static host.
- **Viewer (Dr Rail):** a read-only history and note-lineage viewer, from a mnemonic, a viewing
  key or a legacy shareable viewing key.

## Repository layout

| Crate | What |
|---|---|
| `railgun-wallet` | Wallet daemon (`127.0.0.1:8787`) and its web front |
| `railgun-wallet-web` | The same front as a static site: wallet engine in wasm, Ledger over WebUSB and Web Bluetooth |
| `railgun-viewer` | Dr Rail, read-only viewer daemon (`127.0.0.1:8790`) |
| `railgun-viewer-web` | Dr Rail as a static site |
| `railgun-ledger` | Ledger support: APDU protocol of the ZKNOX Railgun app, `LedgerSigner` (implements the SDK's `RailgunSigner`), transports (USB HID, Bluetooth LE, WebUSB, Web Bluetooth), Ethereum app for the public account |
| `railgun-broadcaster` | Client for the Railgun community broadcaster network |
| `waku-light` | Waku light client (filter, light push) on rust-libp2p, used by the broadcaster client |
| `railgun-chains` | Railgun V2 deployments beyond the SDK's mainnet and Sepolia (BNB Chain, Polygon, Arbitrum) |
| `kv-store-js` | Browser storage (IndexedDB) backend for the SDK's key-value store |

Each app crate has its own README with details.

## The Kohaku SDK

The Railgun protocol code (notes, Merkle trees, proving, POI, 4337 user operations) comes from
[kohaku-rs](https://github.com/ethereum/kohaku-rs), branch `zknox/dev` of the ZKNOX fork. That
branch is upstream's `feat/kohaku-railgun` plus the fixes and features this wallet needs, which
are being proposed upstream as separate PRs.

The SDK is a **git dependency** on [ZKNoxHQ/kohaku-rs](https://github.com/ZKNoxHQ/kohaku-rs),
branch `zknox/dev`: Cargo fetches it, nothing to clone. `Cargo.lock` pins the commit, so
everyone builds the same SDK. To move to the head of the branch:

```sh
cargo update -p kohaku-railgun
```

To work on the SDK and the wallet together, point the build at a local checkout with a
`[patch]` that is not committed, for instance in `~/.cargo/config.toml` (absolute paths). Patch
all six crates, so none is built twice:

```toml
[patch."https://github.com/ZKNoxHQ/kohaku-rs"]
kohaku-crypto = { path = "/path/to/kohaku-rs/crates/crypto" }
kohaku-eip-1193 = { path = "/path/to/kohaku-rs/crates/eip-1193" }
kohaku-kv-store = { path = "/path/to/kohaku-rs/crates/kv-store" }
kohaku-railgun = { path = "/path/to/kohaku-rs/crates/railgun" }
kohaku-userop-kit = { path = "/path/to/kohaku-rs/crates/userop-kit" }
poseidon-rust = { path = "/path/to/kohaku-rs/crates/poseidon-rust" }
```

The patch rewrites the SDK entries of `Cargo.lock`: do not commit them.

## Prerequisites

- Rust (stable).
- For the daemon wallet, whose Ledger transports are built in:
  - USB: libudev headers (`libudev-dev` on Debian/Ubuntu);
  - Bluetooth: D-Bus headers (`libdbus-1-dev`).
- For the static sites:
  - the wasm target: `rustup target add wasm32-unknown-unknown`;
  - `wasm-bindgen-cli` at the lockfile's version: `cargo install wasm-bindgen-cli --version 0.2.108`;
  - `wasm-opt` (optional, smaller output).

## Run

### Daemon wallet

```sh
cargo run --release -p railgun-wallet -- --port 8787 --data-dir ~/.railgun-wallet
```

Open <http://127.0.0.1:8787>. Use `--release`: proving in debug is many times slower.

### Static web wallet

```sh
WASM_BINDGEN="$HOME/.cargo/bin/wasm-bindgen" sh crates/railgun-wallet-web/build.sh
python3 crates/railgun-wallet-web/serve.py        # http://127.0.0.1:8100
```

- Use Chrome or Chromium: WebUSB and Web Bluetooth (for the Ledger) are Chromium-only.
  `127.0.0.1` counts as a secure context; any other host needs HTTPS.
- `LTO=false` speeds up rebuilds while iterating.
- `build.sh` recreates `dist/`: restart `serve.py` after each rebuild.
- `dist/` is ready for Netlify. The build writes `_redirects`, which relays `/poi` to the POI
  node, and `_headers`.

### Viewer (Dr Rail)

```sh
cargo run --release -p railgun-viewer             # daemon, http://127.0.0.1:8790

WASM_BINDGEN="$HOME/.cargo/bin/wasm-bindgen" sh crates/railgun-viewer-web/build.sh
python3 -m http.server -d crates/railgun-viewer-web/dist 8101   # static site
```

## Tests

```sh
cargo test --workspace --exclude railgun-wallet-web --exclude railgun-viewer-web --exclude kv-store-js
```

The excluded crates are wasm-only; check them with
`cargo check --target wasm32-unknown-unknown -p railgun-wallet-web -p railgun-viewer-web`.
The Ledger signer is tested against a software model of the device app
(`crates/railgun-ledger/tests/mock_device.rs`).

## Notes

- **POI node reachability:** the POI node (`ppoi.fdi.network`) resets connections over IPv6.
  The wallets go through a same-origin `/poi` relay (`serve.py` locally, `_redirects` on
  Netlify), so IPv6-only clients still reach it.
- **Local data:** the daemon stores the SDK state in `<data-dir>/<chain>/<id>/db-v3.redb`, the
  static sites in IndexedDB. Both are caches: deleting them only costs a resync.

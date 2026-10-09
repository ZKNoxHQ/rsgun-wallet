# railgun-wallet

Local Railgun wallet built on the kohaku Rust SDK (`crates/railgun`). A daemon bound to
`127.0.0.1` owns the keys and the `RailgunProvider`; an embedded single-page front drives it.

Purpose: stabilise shield, private transfer, unshield and proof of innocence end to end on the
Rust SDK before adding the Railgun community broadcaster transport.

## Run

```sh
cargo run --release -p railgun-wallet -- --port 8787 --data-dir ~/.railgun-wallet
```

Open <http://127.0.0.1:8787>. Use `--release`: Groth16 proving in debug is many times slower.
Log verbosity follows `RUST_LOG` (default `info`).

## Transports

| Tab | State | Path |
|---|---|---|
| 4337 | active on chains with a privacy paymaster (mainnet, Sepolia) | `RailgunProvider::prepare_userop`, fresh 7702 sender per operation, fee note in shielded wrapped base token |
| legacy | active when the Waku node reports a usable offer | `railgun-broadcaster`: fee note pinned first, `minGasPrice`, pre-transaction POIs, sealed request over Waku. The Waku node runs inside the daemon (native light node, `waku-light`) and dials the Railgun fleet over wss on port 8000; the js-waku node of the wallet tab and a local nwaku over REST remain as alternatives, see `crates/railgun-broadcaster/README.md` |
| direct (debug) | active when a public key is loaded | `RailgunProvider::build` then `transact()` from the public EOA. Links the EOA to the transaction |

Shielding is a public transaction and always goes out from the public EOA.

## Keys

Two derivations from a BIP-39 phrase, because they give different 0zk addresses:

* `railgun` (default): Railgun engine scheme, same address as Railway.
* `kohaku`: secp256k1 BIP-32 at the same paths, as kohaku's `MnemonicKeystore` does.

Keys live in the daemon memory only. `<data-dir>/<chain>/<tag>/db-v2` (mode 0700, `src/db.rs`) holds synced commitments,
decrypted notes and pending POI entries (which include the nullifying key, see the upstream TODO
in `poi/provider.rs`). `ephemeral_senders.jsonl` (mode 0600) keeps the key of any 7702 sender
that funds transit through (native unshield), written before the UserOperation is sent.

The 0zk address shown is the chain-agnostic form (`ChainId::All`), the same string Railway
shows for the same keys. The chain field of an address is advisory only; a chain-scoped
address of the same keys receives the same notes.

## API

`GET /api/status`, `POST /api/unlock`, `POST /api/lock`, `POST /api/op`
(`{"op":"sync"|"shield"|"transfer"|"unshield", ...}`), `GET /api/jobs`, `GET /api/jobs/{id}`,
`GET /api/logs?since=N`. Requests with a foreign `Origin` or `Host` are refused.

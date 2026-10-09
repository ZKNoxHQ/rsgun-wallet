# Architecture decisions

## ADR-001: protocol crate, no proving

The crate stops at `BroadcastRequest`: calldata, `minGasPrice` and pre-transaction POIs come in
ready-made. Proving needs `&mut RailgunProvider`, gas estimation needs an RPC; both belong to
the application. The crate depends on `railgun` only for the 0zk address decoder and the
`PreTransactionPois` type.

## ADR-002: own channel cryptography, not `railgun::crypto::SharedKey`

Notes use `sha256(scalar * EdwardsPoint)`. The broadcaster channel uses noble's
`getSharedSecret`, an X25519 exchange with the raw output as key. Found by reading the wallet
SDK while writing this crate; the earlier plan to reuse `SharedKey` was wrong, and the
visibility change made for it in `railgun` 0.3.0 is unused.

## ADR-003: Waku through a local nwaku over REST

No mature Rust implementation of Waku relay or light push exists. nwaku is what broadcasters
themselves run, its REST API is small and stable, and the trait keeps a libwaku FFI or a js-waku
bridge possible. Cost: the user runs a node. `poll()` drains every content topic at once because
that is how the REST cache works; the client routes fees and responses from the single drain.

## ADR-004: one drain, two consumers

The fee monitor and `send()` both call `pump()`. Responses are kept in a short backlog rather
than handed to whoever polled, so a response drained by the monitor is still found by `send()`.
`send()` clears the backlog first: nothing older than the request can be its answer.

## ADR-005: trusted fee signers follow the reference, with one deviation

Implemented as in `waku-broadcaster-client`: authorized rate from the trusted signers (average),
band of 10% under and 30% over with the same integer rounding, filter on arrival and at
selection, nothing usable without an authorized rate. Kept as is, including the consequence that
with several signers far apart, a signer's own offer can fall outside the band of the average.

Deviation: the reference compares lowercase address strings. A 0zk address has a chain-agnostic
and a chain-scoped encoding of the same keys, and broadcasters announce the scoped one, so a
signer configured from what a wallet displays would never match. Signers are compared on
(master public key, viewing public key).

The policy is optional in the crate. Requiring it is the application's call: the wallet requires
it on mainnet only.

## ADR-006: native light node as a transport, behind a feature

`waku-light` removes the two costs of the earlier options: the user running nwaku (ADR-003) and
a browser tab that must stay open (`BrowserBridge`). It is a feature so that the crate's default
build does not pull libp2p for users of `NwakuRest` or the bridge. The adapter maps the node's
state onto the trait without new error variants: not ready is `Remote`, like the bridge, so the
wallet's retry loop is unchanged. Publishing waits for the light push answers, which gives the
caller a definite outcome; the bridge could only report it at the next exchange.

## ADR-007: same client in the browser, with `?Send` futures there only

A pure web wallet needs the whole broadcaster client in the page or its worker, not only the
Waku node. The browser forbids nothing the client does except the clock and timers, handled by a
`time` module on `web-time`, and `Send` futures, which JS-backed objects cannot give. The trait
keeps `Send + Sync` on the object and switches only its futures to `?Send` on wasm32, so native
callers (the daemon's I/O runtime spawns them) are unchanged.

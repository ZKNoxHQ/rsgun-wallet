# Changelog

## 0.7.0 (2026-09-23)

Browser target (wasm32-unknown-unknown), on top of waku-light 0.3.

* `WakuTransport` and its implementations use `async_trait(?Send)` on wasm32, where futures
  built on JS objects are not `Send`; native builds keep `Send` futures.
* New `time` module (`Instant`, `SystemTime`, `sleep`) from `web-time`, tokio natively,
  wasm-bindgen timers in a browser. `BroadcasterClient::send` (republish and response wait),
  `fees::now_ms`, the tab bridge and the nwaku timestamp go through it; `std::time` and
  `tokio::time` are no longer called directly.
* Dependencies split by target: tokio `time` and reqwest `rustls` native only, futures-timer
  (`wasm-bindgen`) wasm only. `light-node` now works on both targets.
* Browser demo: example `web_light_fees` (cdylib) and `examples/web/` (page, build script, port
  8089). The whole `BroadcasterClient` runs in the page: fee announcements authenticated, capped
  by the Railway trusted signers when enabled, listed with their rates. Passive.
* Tests and native examples are `cfg`-gated out of wasm builds; native behaviour unchanged.

## 0.6.1 (2026-09-23)

* `WakuTransport::publish_stats` (default `None`), implemented by `BrowserBridge` and
  `LightNodeTransport`; `BroadcasterClient::publish_stats` forwards it. A caller can now tell,
  whatever the transport, whether a request reached a Waku peer before blaming the broadcaster.
* `LightNodeTransport` logs every light push at info level: accepted by how many peers, and the
  refusals. It was a debug line, invisible in the wallet.

## 0.6.0 (2026-09-23)

* `LightNodeTransport` (feature `light-node`): the Waku node is `waku-light`, a native light
  client on rust-libp2p, instead of js-waku in a browser tab or a local nwaku. `for_chain`
  dials the fleet's wss peers on cluster 5 shard 1 and listens on the fees and
  transact-response topics of the chain. Lazy start on the caller's runtime, stop on drop.
  `subscribe` / `poll` report `Remote` (peers, service nodes, subscriptions, last error) until a
  service node holds the subscription; `publish` is the light push outcome itself, so a refused
  publish is an error at once instead of a missing acknowledgement.
* Example `light_fees`: `BroadcasterClient` on that transport against the fleet, passive,
  prints the authenticated offers and the authorized rates of the trusted signers.
* Tests: fleet configuration, and every call reporting not-ready without any peer.

## 0.5.0 (2026-09-20)

* `BrowserBridge`: publishes carry an id and are acknowledged (`PublishAck`), with counters in
  `publish_stats()`. `exchange` takes the acknowledgements.
* `select_quote`: draw among the offers within a percentage of the cheapest, with an exclusion
  list that is ignored when nobody else is left (`findRandomBroadcasterForToken` of the reference
  client). `best_quote` is unchanged.

## 0.4.1 (2026-09-20)

* `NoQuote::PoiListMismatch`: offers exist but require POI lists the caller does not prove
  against. Before, this case was reported as an empty market.
* `MockBroadcaster::required_poi_list_keys`, and a regression test for list matching.

## 0.4.0 (2026-09-20)

* `BrowserBridge` transport: the Waku node runs elsewhere (js-waku in a browser tab) and calls
  `exchange()` to hand over received messages and take the ones to publish. Reports itself
  unreachable when the remote node stops calling or is not connected; queued publishes expire
  after 30 s.
* `TransportError::Remote`, `wire::{CLUSTER_ID, SHARD_ID, FLEET_WSS_PEERS}`.

## 0.3.0 (2026-09-20)

* `BroadcasterClient::best_quote(token, list_keys, max_rate)` and `FeeCache::best_quote`:
  cheapest usable offer under an optional rate ceiling. `NoQuote::AboveCeiling` reports the
  cheapest rate refused, so a caller can tell an empty market from an expensive one.
* `PAR_RATE_WRAPPED_BASE_TOKEN`: the rate at which the fee equals the gas cost when the fee
  token is the wrapped base token.
* `fake_network` takes the announced rate as a fourth argument, in hundredths of the gas cost
  (default 112).

## 0.2.1 (2026-09-20)

* `RAILWAY_TRUSTED_FEE_SIGNERS`: the four trusted fee signers of the Railway wallet, from its
  remote configuration read on 2026-09-20. Exported, never applied implicitly. A unit test checks
  that each entry is a valid 0zk address and that the four keys are distinct.

## 0.2.0 (2026-09-19)

* Trusted fee signers, after `trustedFeeSigner` of the reference client:
  `BroadcasterClient::with_trusted_signers`, `TrustPolicy`, `authorized_fees()`.
  * A trusted signer's announcements set the authorized rate per token, averaged over the
    signers with a live announcement (one expiring within 40 s does not count).
  * Other broadcasters are kept only within `[authorized - 10%, authorized + 30%]`, checked on
    arrival and again at selection, since the authorized rate moves.
  * With signers set and no authorized rate for a token, no offer is usable for it.
  * Signers are matched on their keys, not on the address string, so a chain-agnostic address
    in the configuration matches a chain-scoped one on the wire.
* `FeeError::TrustedSigner` for a configuration entry that is not a 0zk address.

## 0.1.0 (2026-09-19)

* First version: wire format, channel cryptography, fee cache and selection, client with retry
  schedule, nwaku REST transport, in-memory hub, mock broadcaster, `fake_network` example.

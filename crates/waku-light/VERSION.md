# Changelog

## 0.4.1 (2026-09-23)

* Echo test validated natively against the fleet: 3/3 light pushes accepted (lightpush v3),
  3/3 echoed, median round trip 821 ms. A fleet node that had just joined answered v3 status 505
  ("No peers for topic, skipping publish"), counted as a refusal for that peer while another
  accepted: the v3 status codes tell a node that cannot relay from one that did.
* Browser demo hosted in a dedicated Web Worker (`examples/web/worker.html`, `worker.js`), the
  form a web wallet takes: node, polling and echo test run in the worker, the page renders what
  it posts. Shows the largest gap between two of the worker's 1 s ticks, to measure throttling
  when the tab is in the background. Exercises the worker branch of `libp2p-websocket-websys`.

## 0.4.0 (2026-09-23)

Light push check without any broadcaster involved.

* `PublishReport::accepted_via`: `peer (lightpush v3|v2)` for each peer that accepted, so a
  publish says which protocol version the fleet took.
* Example `echo` (native): subscribes to `/zknox-waku-light/1/echo/json`, a content topic outside
  every Railgun topic, publishes timestamped messages on it and waits for each to come back
  through filter (light push to the fleet, relay between its nodes, filter push back). Reports
  acceptance per peer and the round trip.
* Browser demo: "Echo test" button, same test from the page; `WebNode::subscribe` and
  `WebNode::publish` (Promise with the report).

## 0.3.1 (2026-09-23)

* Browser demo: example `web_fleet` (cdylib, wasm-bindgen) and `examples/web/` (page, build
  script). The page starts a `LightNode` over the browser's WebSocket, subscribes to the fees
  topic of a chosen chain and lists the broadcasters it hears (address, version, wallets,
  reliability, tokens, offer expiry). Passive, publishes nothing. Built with
  `examples/web/build.sh` (wasm32 target, wasm-bindgen-cli 0.2.108 as pinned by the workspace),
  served with `python3 -m http.server`.
* On native targets the example compiles to an empty library, so `cargo test` is unaffected.

## 0.3.0 (2026-09-23)

Browser target (wasm32-unknown-unknown), compile step. Native behaviour unchanged.

* Transport chosen by target: native keeps libp2p's WebSocket over DNS + TCP with rustls; wasm32
  uses `libp2p-websocket-websys`, the page's or worker's own `WebSocket`, so DNS and TLS
  (TLS 1.2 to the fleet's BearSSL included) are the browser's. Noise, yamux / mplex, identify,
  ping, `libp2p-stream` and all Waku protocol code are shared.
* Swarm executor: tokio natively, `with_wasm_executor()` in a browser.
* `rt` on wasm32: `spawn_local`, `futures-timer` (wasm-bindgen timers) for sleep, timeout and
  interval, `MaybeSend` without `Send`.
* Dependencies split by target: tokio `rt`/`time`, DNS, TCP, rustls and hickory are native only;
  websocket-websys, futures-timer (`wasm-bindgen`), wasm-bindgen-futures and the JS backends of
  getrandom 0.2 / 0.3 / 0.4 are wasm only. tokio keeps `sync` and `macros` on both.
* Tests and examples that need tokio or a listener are native only (`cfg`); the 12 native tests
  are unchanged.
* To validate on a machine with the wasm32 target:
  `cargo check -p waku-light --target wasm32-unknown-unknown`. The functional browser test
  (a page listening to the fleet) is the next step.

## 0.2.0 (2026-09-23)

First step towards a browser build (pure web wallet). No behaviour change on native targets.

* New `rt` module, the only place that names the executor and the clock: `spawn` (returns a
  `Task` aborted through `futures::Abortable`), `sleep`, `timeout`, `Interval`, and `Instant` /
  `SystemTime` from `web-time`. Native implementation on tokio, as before; the node, filter,
  light push and metadata code no longer calls `tokio::time`, `tokio::spawn` or `std::time`.
* tokio's `rt` and `time` features are now native-only dependencies; `sync` and `macros`
  (`Notify`, `select!`) stay common, both build for wasm32.
* Tests: `rt::abort_stops_a_task`, `rt::timeout_reports_elapsed`; the 10 earlier tests unchanged.
* Next: websys transport and the wasm32 side of `rt` (0.3.0), validated with `cargo check
  --target wasm32-unknown-unknown`, a wasm client in Node 22 against the loopback service node,
  then a static page listening to the fleet.

## 0.1.4 (2026-09-23)

* Validated against the Railgun fleet (example `fleet`, Sepolia): 10 fee announcements from 3
  broadcasters in 60 s, no duplicate, relay-b and client-edge subscribed within 4 s. All three
  fleet nodes present identities other than the ones the reference client pins.
* `Config::dial_timeout`, 45 s by default instead of a fixed 20 s: relay-a took about 10 s to
  complete a dial and went past 20 s twice in a row.
* `Status::last_error` is cleared when a filter subscription succeeds, so a notice about an
  earlier dial or a dropped pin does not stay on a working node.

## 0.1.3 (2026-09-23)

* TLS and noise now pass against the fleet (0.1.1 fix confirmed). New failure on relay-a:
  `Unexpected peer ID 16Uiu2HAmMkCL…9AsR`, where the reference client, including 10.0.0 published
  on 2026-09-22, still pins `16Uiu2HAmFbD2…tG7x`. The fleet rotated that node's key.
* A pinned `/wss` bootstrap peer that answers with another identity is kept: the pin is
  dropped, logged at warn level with both ids, and the address is redialled at once without it.
  The certificate already authenticated the host. `Config::accept_rotated_wss_identity`
  (default true) turns this off.
* `/p2p/<id>` may be left out on `/wss` bootstrap addresses; it stays mandatory on any other
  address, and a pin on a non-wss address is always enforced (test `wrong_pin_on_ws_is_refused`).
* Dials are tracked by connection id, so an unpinned dial is matched to its bootstrap entry.

## 0.1.2 (2026-09-23)

* Example `probe`: dials one multiaddr and logs how far the connection gets (TLS, noise, muxer,
  identify, filter). For peers other than the fleet defaults, and for diagnosis.
* The TLS 1.2 fix of 0.1.1 was checked against the BearSSL that nwaku links (status-im/BearSSL
  7bea48e, `brssl server`, EC key, TLS 1.1 to 1.2, same negotiation as the fleet:
  ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 over Curve25519). Without `tls12` the dial ends in
  `received fatal alert: HandshakeFailure`, as reported on the fleet; with it the handshake
  completes.

## 0.1.1 (2026-09-23)

* Fix: every wss dial to the Railgun fleet failed with `received fatal alert: HandshakeFailure`.
  libp2p-websocket builds rustls without its `tls12` feature, so the client offered TLS 1.3
  only, and nwaku serves wss through nim-websock on BearSSL, which stops at TLS 1.2. `rustls`
  is now a direct dependency with `tls12`, which Cargo unifies into the one libp2p uses.
* Test `wss_client_offers_tls12` so the feature cannot silently drop out again.

## 0.1.0 (2026-09-23)

* Light node on rust-libp2p 0.57: WebSocket (ws/wss) over DNS and TCP, noise, yamux or mplex,
  identify, ping.
* Filter v2 client: subscribe, unsubscribe, unsubscribe all, subscriber ping every 60 s,
  resubscription when a ping or a subscribe fails, content topics sent in chunks of 100.
* Inbound filter push: pubsub topic and content topic checked, deduplicated by the RFC 14
  deterministic hash (several service nodes push the same message), bounded inbox.
* Light push v3, v2 when the peer does not announce v3 or refuses it as unsupported; sent to up
  to 3 peers at once, `PublishReport` with accepted count and per-peer failures.
* Metadata v1 served and queried; a peer on another cluster is ignored.
* Bootstrap peers only, redial with backoff 5 to 60 s. DNS falls back to Cloudflare without a
  system configuration.
* Protobufs derived by hand with prost, no protoc.
* Example `fleet`: passive listen on the Railgun fleet.

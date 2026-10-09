# waku-light

Native Waku light client on rust-libp2p, written to replace the js-waku node that the Railgun
wallet runs in its browser tab. It speaks what the Railgun fleet expects from a light node, and
nothing else.

## Protocols

| Protocol | Direction | Use |
|---|---|---|
| `/vac/waku/filter-subscribe/2.0.0-beta1` | out | subscribe, unsubscribe, subscriber ping (60 s) |
| `/vac/waku/filter-push/2.0.0-beta1` | in | messages pushed by the service nodes |
| `/vac/waku/lightpush/3.0.0` | out | publish, when the peer announces it |
| `/vac/waku/lightpush/2.0.0-beta1` | out | publish, fallback |
| `/vac/waku/metadata/1.0.0` | in and out | cluster and shards (nwaku drops peers that do not answer) |
| `/ipfs/id/1.0.0`, `/ipfs/ping/1.0.0` | both | protocol discovery, liveness |

Transport: WebSocket (`/ws`, `/wss` with rustls and the webpki roots) over DNS and TCP, noise,
yamux or mplex (the js-waku bundle only offers mplex, nwaku accepts both). Without a system DNS
configuration (Android), DNS falls back to Cloudflare's resolvers.

No relay, no store, no discovery: only the bootstrap peers are dialled, and redialled with a
backoff of 5 to 60 s when they drop.

## API

```rust
let node = LightNode::start(Config::new(bootstrap, 5, 1))?;   // cluster 5, shard 1
node.subscribe(["/railgun/v2/0-11155111-fees/json"]);
node.wait_subscribed(Duration::from_secs(60)).await;
let messages = node.drain();                                  // deduplicated, oldest first
let report = node.publish(topic, payload).await?;             // report.accepted, report.failures
let status = node.status();                                   // peers, service peers, subscriptions
```

`start` spawns on the current tokio runtime. Dropping the node stops everything.

## Targets

* Native (Linux, macOS, Android): tokio, libp2p WebSocket over TCP + DNS, rustls.
* Browser (wasm32-unknown-unknown, window or Web Worker): the browser's own WebSocket
  (`libp2p-websocket-websys`), which leaves DNS and TLS to the browser; `spawn_local` and
  wasm-bindgen timers. `cargo check -p waku-light --target wasm32-unknown-unknown`.

Light push check, native, touching no broadcaster (publishes on a content topic of its own and
waits for the fleet to push it back):

```sh
cargo run --release -p waku-light --example echo -- 3
```

Browser demo (lists the broadcasters announcing fees on the chosen chain; its "Echo test" button
runs the same light push check from the page; `worker.html` runs the same node in a dedicated Web
Worker):

```sh
cargo install wasm-bindgen-cli --version 0.2.108 --locked   # once, matches the workspace pin
crates/waku-light/examples/web/build.sh
python3 -m http.server 8088 -d crates/waku-light/examples/web   # then http://localhost:8088
```

Everything platform-dependent goes through the `rt` module (executor, timers, clock) and the
`base_transport` / `swarm_config` pair in `node.rs`; the Waku protocols are shared.

## Checking against the Railgun fleet

Passive: subscribes to the fees topic and prints what arrives, publishes nothing.

```sh
cargo run --release -p waku-light --example fleet -- 11155111 60
RUST_LOG=waku_light=debug,libp2p=debug cargo run --release -p waku-light --example fleet -- 11155111 60
```

## Tests

`cargo test -p waku-light`: framing, protobuf encodings, deduplication, and two loopback tests
where a fake service node (secp256k1 identity, `/ws` listener) serves filter, light push v3 or
v2 only, and metadata to a real `LightNode`.

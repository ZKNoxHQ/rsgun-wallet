# Architecture decisions

## ADR-001: a Waku light client of our own on rust-libp2p

No Rust Waku client exists; the alternatives were a local nwaku (the user runs a node), libwaku
over FFI (Nim toolchain, heavy on Android) or js-waku in a browser tab (what the wallet did until
now, the tab must stay open). A light node only needs four request/response protocols on top of
libp2p, each a single length-prefixed protobuf per stream direction, so writing them is small and
removes the JavaScript dependency.

## ADR-002: raw streams (`libp2p-stream`), not `request-response`

Waku protocols open one stream per exchange, and filter push is inbound without an answer. With
`libp2p-stream` each protocol is a short async function over a stream, callable from any task
through a cloned `Control`; the swarm task only dials, tracks connections and reads identify.

## ADR-003: yamux and mplex, both offered

The js-waku bundle negotiates mplex only, which proves the fleet accepts it; nwaku also speaks
yamux. Offering yamux first and mplex second keeps the maintained muxer while staying compatible
with a node that would only take mplex.

## ADR-004: metadata served and queried

On a sharded cluster nwaku queries every new peer's metadata and disconnects on failure or on
a different cluster. Shards are written unpacked, as nwaku writes them.

## ADR-005: deduplicate by the deterministic message hash

The node subscribes with every connected service node for resilience, so each message arrives
several times. The RFC 14 hash (pubsub topic, payload, content topic, meta, timestamp) is the
identity nwaku itself uses.

## ADR-006: TLS 1.2 enabled for wss

libp2p-websocket depends on rustls with only `ring` and `std`; its client config asks for the
safe default protocol versions, which without `tls12` means TLS 1.3 alone. The fleet's wss
endpoints are nwaku over nim-websock and BearSSL, TLS 1.2 at most: the server answers the
ClientHello with a handshake_failure alert. Browsers were fine because they offer both. We turn
the feature on from this crate rather than build our own TLS config, which would mean replacing
the websocket transport's TLS setup for one flag. rustls still refuses the weak TLS 1.2 suites
(no CBC, no RSA key exchange); if a peer only offered those, the dial would fail the same way.
Reproduced locally with `brssl server` built from status-im/BearSSL (the copy vendored by
nim-bearssl): HandshakeFailure without the feature, full handshake with it.

## ADR-007: a rotated identity on a wss bootstrap peer is accepted, not refused

relay-a.rootedinprivacy.com answers with a libp2p key other than the one every copy of the
reference client pins, up to 10.0.0. Refusing it would cost one of three fleet nodes until a new
client release, and a release would not help nodes already shipped. Over `/wss` the TLS
certificate, checked against the webpki roots, already authenticates the host name the fleet
operator controls; the libp2p pin adds protection only against someone holding a valid
certificate for that name. On `/ws` or `/tcp` the pin is the only authentication, so there it is
enforced. The change of identity is logged with both ids so that a real key change can be told
apart from an attack after the fact.

## ADR-008: one runtime seam for native and browser builds

A browser has no tokio timer driver, no `std::time::Instant` (it panics on
wasm32-unknown-unknown) and no threads, so its futures need not be `Send`. Rather than scatter
`cfg(target_arch)` through the node, everything that depends on the platform goes through `rt`:
spawn, sleep, timeout, interval, clock. Task cancellation uses `futures::Abortable` on every
target, so dropping a `LightNode` behaves the same whatever the executor. `MaybeSend` is `Send`
on native targets and will be empty on wasm32, where `spawn` maps to `spawn_local`.

The protocol code (framing, protobufs, filter, light push, metadata, deduplication, identity
handling) is target-independent already. What remains target-specific after this seam is the
transport (TCP + DNS + rustls natively, the browser's WebSocket in wasm) and the swarm executor.

## ADR-009: the browser build uses the browser's WebSocket, not libp2p's

In a page there are no sockets, so `libp2p-websocket` (TCP underneath) cannot work;
`libp2p-websocket-websys` wraps `web_sys::WebSocket` instead. The browser then does DNS and TLS,
which removes the rustls TLS 1.2 question (ADR-006) on that target: browsers negotiate TLS 1.2
with the fleet's BearSSL, as js-waku shows. The pin logic (ADR-007) is unchanged, since noise
still runs inside the WebSocket and reports the remote's libp2p identity.

The transport works in a window or a Web Worker (it looks for either global scope). A worker is
the intended host for a wallet, since background tabs throttle timers; the filter ping period of
60 s tolerates the one-minute floor anyway.

Randomness: three getrandom generations are in the graph (0.2 via rand 0.8 in libp2p-identity,
0.3 via snow in noise, 0.4 via k256); each gets its JS backend feature on wasm32, and 0.3 also
the `getrandom_backend` cfg the workspace already sets for its other wasm crates.

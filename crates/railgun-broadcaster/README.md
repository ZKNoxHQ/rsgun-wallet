# railgun-broadcaster

Rust client for the Railgun community broadcaster network, the "legacy" relay transport: a
broadcaster submits the shielded transaction from its own EOA and is paid by a note inside it.
Protocol follows `@railgun-community/waku-broadcaster-client` 9.x and `shared-models`.

## What it does

* `wire`: content topics (`/railgun/v2/0-<chain>-{fees,transact,transact-response}/json`), relay
  shard `/waku/2/rs/5/1`, message types.
* `crypto`: the wallet to broadcaster channel. `@noble/ed25519` v1 `getSharedSecret` (X25519 on
  the Montgomery form of ed25519 keys, raw output as AES key), AES-256-GCM with a 16-byte nonce
  over JSON, ed25519 verification of fee messages. Checked against vectors generated with noble
  1.7.3 and Node crypto.
* `fees`: authentication of announcements with the viewing key of the announcing 0zk address,
  version range, expiry margin of 40 s, cache, ranking, and `token_fee`
  (`feePerUnitGas * gasEstimate * 1.2 * gasPrice / 1e18`).
* Trusted fee signers (`TrustPolicy`): offers are capped to 10% under and 30% over the rate
  announced by configured 0zk addresses, as in the reference client. Not set by default;
  `RAILWAY_TRUSTED_FEE_SIGNERS` holds the list the Railway wallet uses.
* Rate ceiling (`best_quote`): for the wrapped base token the par rate is known, so a multiple
  of `PAR_RATE_WRAPPED_BASE_TOKEN` bounds any broadcaster without trusting anyone.
* `client`: `pump()` to keep the cache current, `seal()` with a fresh ephemeral key per request,
  `send()` with the reference retry schedule (republish every 2 s for 20 s, listen up to 120 s).
* `transport`: the `WakuTransport` trait, `LightNodeTransport` (native light node, feature
  `light-node`), `BrowserBridge` (node in a browser tab, js-waku), `NwakuRest` (local nwaku), and
  an in-memory hub for tests.

What the wallet still does itself, because it owns the `RailgunProvider` and the RPC: gas price,
dummy-proof gas estimate, `broadcaster_fee` + `min_gas_price` on the builder, proof,
pre-transaction POIs. See `railgun-wallet/src/engine.rs::submit_legacy`.

## Waku node

### Native light node (feature `light-node`)

`LightNodeTransport::for_chain(chain_id)` runs the `waku-light` crate in process: filter v2 to
receive, light push v3 or v2 to send, metadata, over wss to the Railgun fleet on `:8000`. Nothing
to install, no browser tab, no JavaScript. It starts on the first call on the caller's tokio
runtime and stops when dropped. Until a service node holds the filter subscription, `subscribe`
and `poll` return `TransportError::Remote` with the peer count and the last error, which the
wallet's fee monitor retries every 3 s.

Passive check against the fleet (prints the authenticated offers, publishes nothing):

```sh
cargo run --release -p railgun-broadcaster --features light-node --example light_fees -- 11155111 90
```

### In a browser (wasm32)

The crate and `LightNodeTransport` build for `wasm32-unknown-unknown`: the light node then uses
the browser's WebSocket (see `waku-light`). Demo page running the whole client, passive:

```sh
crates/railgun-broadcaster/examples/web/build.sh
python3 -m http.server 8089 -d crates/railgun-broadcaster/examples/web   # http://localhost:8089
```

### js-waku in a browser tab, local nwaku

`BrowserBridge` hands messages to and from a js-waku node running in the wallet's tab. The rest
of this section is the nwaku alternative.

The `NwakuRest` transport talks to a local
[nwaku](https://github.com/waku-org/nwaku) over REST. Start one on the Railgun shard, for example:

```sh
docker run --rm -p 8645:8645 wakuorg/nwaku:latest \
  --cluster-id=5 --shard=1 --num-shards-in-network=6 \
  --relay=true --rln-relay=false --max-msg-size=512KiB \
  --rest=true --rest-address=0.0.0.0 --rest-port=8645 --rest-admin=true \
  --rest-relay-cache-capacity=3000 \
  --dns-discovery=true \
  --dns-discovery-url=enrtree://APMYHUVNQWHJNPI5L2KQ765EMCKUAMRWPUH3U2QIKPK6XEV3OW442@discovery.rootedinprivacy.com
```

Cluster, shard and ENR tree come from the reference client's constants; the shard is confirmed by
Railway's remote configuration (`wakuPubSubTopic`), which lists no additional direct peer. The command itself has
not been run here: flag names change between nwaku releases, check `--help` of the image you
pull. The wallet shows the peer count and the offers it receives, which tells at once whether the
node is on the right shard.

## Without the network

```sh
cargo run -p railgun-broadcaster --example fake_network -- 8645 11155111
```

serves the same REST subset from memory with one mock broadcaster that announces a fee and
answers every request with a made-up hash. Nothing reaches a chain. Useful to work on a wallet's
legacy path up to the proof.

## Tests

`cargo test -p railgun-broadcaster --lib --tests`: interop vectors, fee arithmetic (including
the u128 overflow case), and wallet/broadcaster exchanges over the in-memory hub (forged or
out-of-range announcements dropped, request opened only by its addressee, refusal and timeout
told apart).

Validated against the live Railgun fleet on Sepolia (2026-09-20), through the wallet and its
js-waku tab node: offers received and authenticated, Railway's trusted signers announcing, a
private transfer and a token unshield relayed by a broadcaster, pre-transaction POIs accepted,
post-transaction POIs validated for the change notes.

Not validated: mainnet, the nwaku REST transport against a real node, native unshield (needs
RelayAdapt, not built by the SDK yet), a fee token other than the wrapped base token.

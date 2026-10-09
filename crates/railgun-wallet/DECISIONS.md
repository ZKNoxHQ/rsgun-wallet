# Architecture decisions

## ADR-001: native daemon plus web front, not wasm in the browser

`crates/railgun-ts` already exposes the SDK to the browser. The wallet uses the native crate
behind a loopback HTTP API instead. Reasons: native Groth16 proving (and the `parallel` feature)
is much faster than wasm; the on-disk database survives restarts; the next step, the legacy
broadcaster transport, is a native Rust crate talking to a Waku node, which is awkward from wasm.
Cost: key material crosses a localhost HTTP call at unlock. Mitigated by binding to 127.0.0.1,
refusing foreign `Origin`/`Host`, a strict CSP on the front, and never persisting keys.

## ADR-002: single engine actor on its own thread

`RailgunProvider` needs `&mut self` for sync, balance and build, and its futures are not
guaranteed `Send`. One actor on a current-thread runtime serialises all access. HTTP reads are
served from a snapshot the actor refreshes after every command, so the front stays responsive
during a proof. Consequence: one operation at a time; a sync requested during a proof queues.

## ADR-003: transport is a per-operation parameter

`Transport { Erc4337, Legacy, Direct }` is passed with each transfer/unshield rather than fixed
at unlock. A proof binds `adaptContract`/`adaptParams` to one transport (the fee adapter requires
`adaptParams == sender`, Railgun requires `adaptContract == msg.sender`), so switching transport
means proving again, never resubmitting. `Session::submit` is the single place to add `Legacy`.

## ADR-004: `direct` transport kept as a debug path

Not requested by the product, but it isolates SDK bugs (note selection, proof, POI) from
paymaster and bundler behaviour while stabilising. Labelled as debug in the front because it
links the public EOA to the private transaction.

## ADR-005: two mnemonic derivations

kohaku derives Railgun keys with standard secp256k1 BIP-32; the Railgun engine uses a
hardened-only HMAC-SHA512 chain keyed with "babyjubjub seed". Same phrase, different 0zk
address. Default is the Railgun scheme so existing Railway wallets open with their funds.

## ADR-006: minimal fork surface in `crates/railgun`

Only read-only accessors were added, grouped under a `ZKNOX fork` marker, to keep rebases on
upstream trivial and to make the later PR a pure addition.

## ADR-007: own `Database` implementation instead of patching `kohaku-db`

`FilesystemDatabase` stores each entry as `hex(key)`. Indexer keys embed the 0zk address (127
characters), so the file name passes 255 bytes and `register` fails on ext4, APFS and NTFS. The
trait only has get/set/delete, no listing, so hashing the key loses nothing. Implemented in the
wallet (`src/db.rs`) rather than in `kohaku-db` to keep the fork surface small; the upstream fix
is a separate PR. Writes are atomic because the store holds Merkle trees and pending POI
entries, and a truncated file would force a full resync or lose a proof to submit.

## ADR-008: POI recovery from chain data, inside the SDK fork

Upstream only proves POI for operations registered at build time (`register_ops`). After a
restore, or for transactions made elsewhere, change notes stay `Missing` forever. The Railgun
community wallet recovers because its engine keeps spent TXOs, sent commitments and all railgun
transactions. The fork keeps the minimum equivalent: spent notes, sender-decrypted outputs and
the operations spending our nullifiers (not every operation: on mainnet that would be tens of
megabytes). The receiver master key encoding differs between the Railgun engine (XOR with the
sender key unless hidden) and kohaku (plain); both candidates are tried and the on-chain
commitment hash decides, so annotation data never has to be parsed.

Limits: the unshield-only operation without change is not probed (no note of ours depends on
it); an operation whose outputs straddle two UTXO trees is skipped; an operation spending the
change of another unproved one only succeeds after the POI node has accepted the first, that is
on a later sync.

The logic lives in `crates/railgun` because it needs the indexers' private state. This is the
first non-additive fork patch: `PoiProvider::sync_to` takes the account history, and
`IndexedAccountState`/`TxidIndexerState` gain serde-defaulted fields.

## ADR-009: broadcaster prerequisites are opt-in builder features

The five SDK changes needed by the legacy transport are added without touching the ERC-4337
path: `prepare_userop` calls none of them. `minGasPrice` stays 0 there on purpose, the Railgun
contract compares it to `tx.gasprice`, which is the bundler's. The fee pin is a separate builder
method rather than a change to `transfer(.., "fee")`: the fee adapter scans every commitment, so
4337 needs no ordering, while broadcasters read `transactions[0].commitments[0]` only. The pin
is implemented as a stable sort key, so a builder without it produces the historical order,
which a unit test freezes.

The dummy proof is a distinct entry point and is never used inside the `prepare_userop` loop:
the bundler simulates `validatePaymasterUserOp`, which verifies the SNARK with its own origin.

Pre-transaction POI follows the Railgun engine (`createDummyMerkleProof`, pre-inclusion
position 199999). Accepted by live broadcasters on Sepolia on 2026-09-20, for a transfer and for
an operation with an unshield (`railgunTxidIfHasUnshield`).

## ADR-010: Waku I/O on the server runtime, not the engine thread

The engine thread's runtime is blocked while a proof is computed, so the fee monitor would
starve and offers would expire unseen. The monitor and `send()` run on the HTTP server's
multi-thread runtime through a `Handle` given to the engine; the engine awaits the join handle.
The live broadcaster state sits in `Shared::legacy`, read directly by `/api/status`, because the
engine snapshot is only refreshed after a command.

## ADR-011: fee from two dummy-proof estimates

The fee is an output note, so it changes the inputs selected and possibly their number, hence
the gas. Round one uses a guess (700k gas) to get a realistic estimate, round two re-estimates
with a fee of the right size. The final proof is not re-estimated: the 20% gas limit margin in
the fee formula and the broadcaster's own variance buffer absorb the difference.

## ADR-012: trusted fee signer mandatory on mainnet only

The reference client requires a trusted signer everywhere. Here it is required where real funds
are at stake and optional on test networks, so the legacy path can be exercised on Sepolia
without knowing who signs rates there. No default signer is shipped: the address in use by the
Railgun wallets was not verified, and a wrong default would silently disable the transport.

## ADR-013: Railway's trusted fee signers as the default

The four addresses come from the `trustedFeeSigner` field of Railway's remote configuration
(`https://www.railway.xyz/config/railway-config-v3.3.json`, read on 2026-09-20). They are in no
GitHub repository: Railway loads them at start-up, and applies them to every network. The same
file confirms the relay shard `/waku/2/rs/5/1` and lists no additional direct peer.

Shipped as a prefilled, editable field rather than a hidden default, because it delegates the
reference rate to one wallet team and because the list can change upstream without this code
knowing. It is a snapshot: if legacy offers stop being accepted on mainnet, compare with the live
file first. Supersedes the "no default signer" part of ADR-012.

## ADR-014: fee ceilings that trust nobody

The legacy fee token is the wrapped base token, so the par rate is known: one base unit per wei
of gas (`PAR_RATE_WRAPPED_BASE_TOKEN`). The reference broadcaster adds a margin of 10 to 30%.
A ceiling at 1.5 times par therefore refuses a predatory or misconfigured broadcaster without
an oracle and without trusted signers, on every chain, and also when a trusted signer itself is
compromised. It complements the trust policy, it does not replace it: the band protects against
collusion at rates under the ceiling, the ceiling against everything above.

The absolute maximum covers what the rate cannot: a large gas estimate or a gas price spike.
It is checked after the dummy-proof estimate, the first moment the fee is known, and before the
proof, the first expensive step.

Both limits travel with the operation rather than the session: they are a per-payment decision,
and the front shows the offers they exclude while the user edits them. The wallet still does not
ask for a confirmation between quote and broadcast; the limits are what makes that acceptable.

Limit: this reasoning only holds for the wrapped base token. Another fee token needs a price.

## ADR-015: Waku light node in the wallet tab

Requested over a Node sidecar, to need nothing but the wallet binary and a browser. js-waku is
the only maintained light client and is what Railgun web wallets use. The tab is a dumb pipe:
it moves opaque payloads between the fleet and `BrowserBridge`; authentication of offers,
encryption of requests and every decision stay in the daemon. A hostile page cannot reach
`/api/waku/exchange` (Origin and Host checks), and what it could inject there would be unsigned
offers, dropped at verification.

Accepted cost: the legacy transport only works while the tab is open. Offers stop being
refreshed when it closes, and a broadcaster's answer arriving then is lost; the operation then
ends on the time-out, which already tells the user to sync and check the inputs before retrying.

No DNS discovery and no peer exchange, unlike the reference client: three fixed wss peers keep
`connect-src` to one domain and the bundle small. If the fleet changes its peers the constant
`FLEET_WSS_PEERS` has to follow; Railway's remote configuration lists no additional peer today.

The bundle is committed as a build artefact so that `cargo build` needs no Node toolchain.
Rebuild: `cd crates/railgun-wallet/waku-bridge && npm ci && npm run build`.

## ADR-016: one proof on the 4337 path, by fixing the gas limits first

`prepare_userop` re-proves until the fee matches the bundler's estimate, because the estimate
needs a valid proof and the proof binds the fee. Each round is a spending signature over
(merkle root, bound params, nullifiers, commitments): up to five confirmations on a Ledger, five
ceremonies with FROST.

A dummy proof cannot replace the rounds: the bundler simulates `validatePaymasterUserOp`, whose
caller must be the EntryPoint, so `tx.origin` cannot be the verification bypass address. What a
dummy proof can measure is the bare `transact` call. The paymaster verification limit is that
gas plus an overhead (fee note check, price quote, decoding) which depends on calldata size,
learned once per shape. The other limits barely move for a given shape and come from the same
profile.

The privacy paymaster requires `fee >= quote(maxCost)`, `maxCost` being limits times
`maxFeePerGas`, so choosing the limits determines the fee exactly. Cost of the approach: the
margin, never refunded (the Railgun adapter sets no refund recipient). Failure modes and their
handling:

* limits too low: detected on the real proof before sending; costs a second signature;
* tail calls out of gas after a native unshield: the only case that strands funds, on the
  ephemeral sender whose key the wallet records; the call limit is learned from real
  transactions of the same shape and padded by the margin;
* the fee is public in `paymasterData` and fee / maxFeePerGas gives the total gas. Limits padded
  by a fixed margin tell a single-proof wallet from an iterative one. Rounding to 10k gas hides
  the exact transaction, not the wallet family. Only a rule shared by all wallets would.

Not done: a gas table shipped with the wallet, which would remove the learning transaction. It
needs measurements on each chain. And `RailgunSigner` still exposes `spending_key()`, which a
hardware signer cannot implement: `TransactCircuitInputs` only needs the public key, a change
for the hardware integration itself.

## ADR-017: simulate the UserOperation ourselves instead of learning gas profiles

Supersedes the profile part of ADR-016. A learned profile is configuration: it can be missing
(first transaction of a shape), stale (contract upgrade) or wrong for a chain, and nothing in it
could not be measured.

The obstacle was the origin: the paymaster only accepts the EntryPoint as caller, and the Railgun
verifier only accepts a dummy proof when `tx.origin` is its bypass address. A bundler simulating
for us picks its own origin. An `eth_call` of our own does not have that problem: state override
replaces the EntryPoint's code by a probe that calls the account and the paymaster as the
EntryPoint would, and the call is sent from the bypass address. This is the technique bundlers
use with `EntryPointSimulations`, with a probe of 1.7 kB written for the purpose rather than the
full contract, whose immutables do not survive being installed by override.

What stays an estimate: `preVerificationGas` is the bundler's price for inclusion, not an
execution cost, so no simulation yields it. The reference formula plus a double margin is used,
and the bundler's own figure is checked after the proof. If a bundler prices it higher than the
formula by more than that margin, the operation stops before sending, at the cost of one
signature; the margin is then the knob.

Dependency: an RPC that supports state overrides in `eth_call` (geth, reth, erigon, Nethermind
and the hosted providers built on them do). Without it there is no way to sign once, which is
what strict mode makes explicit.

## ADR-018: one constant after all, for the EntryPoint's own validation overhead

ADR-017 claimed nothing was configured. The first live run showed one figure the probe cannot
produce: what the EntryPoint spends around the account's validation and charges to
`verificationGasLimit`. Measuring it would take the real EntryPoint in the loop, which brings
back the origin problem the probe exists to avoid (a dummy signature makes `handleOps` revert,
and `EntryPointSimulations` does not survive installation by override with its immutables).

It is kept as a named constant in `userop_kit::validation_probe`, with the measurement it comes
from. It depends on the EntryPoint version only, is small next to the paymaster validation
(60k of about 2M gas, 3% of the fee), and the bundler's estimate is still compared after the
proof. It differs in kind from the removed profiles: those were per shape and per chain, and
could be absent.

## ADR-019: a time-out must say which side failed

With the Waku node in the tab, the daemon only queued publishes: whether light push succeeded was
known to the page alone. Every failure then surfaced as "the broadcaster did not answer", which
hides three different things: the request never left, the broadcaster ignored it, or its answer
was lost. The first is now told by acknowledgements, the last by the input notes on-chain
(0.9.2), and what remains is the broadcaster. Only that case penalises the broadcaster.

Always picking the cheapest offer was a mistake of the first version: with two offers a few
percent apart, every request went to the less reliable one. The draw within 10% is the reference
behaviour. No automatic retry on another broadcaster: the fee note is addressed to the
broadcaster, so a retry is a new proof and, with a hardware signer, a new signature the user has
to approve.

## ADR-020: limits are searched, not derived from gas used, where the two diverge

Gas used underestimates the limit a phase needs. For the paymaster validation the gap is the
63/64 rule over a few nested calls, 8% on two live runs, inside the margin. For the execution
call it can be a factor of three: value transfers require gas they do not consume. A margin on
the wrong quantity is not a safety margin.

The probe therefore finds the minimal call limit by trial, on-chain semantics included, inside
the simulation (each trial reverted). This is what bundlers do with a binary search. It is only
done for the call phase: a trial of the paymaster phase costs 1.4M gas, ten of them would exceed
what public RPCs allow in an `eth_call`, and there the measured gap is small and stable.

The call limit is also the one whose shortfall is dangerous: the unshield happens during
validation, so an execution that runs out of gas leaves the funds on the ephemeral sender. Hence
twice the margin on it, and the strict comparison with the bundler's figure after the proof.

## ADR-021: the bundler's call limit is policy, like its pre-verification gas

Second correction to the idea that everything can be measured. The probe measures what the chain
needs; the bundler's estimate is what the bundler wants, and for the call phase the two differ
by a factor of 2.7 on the only case observed (25.3k needed, 68,136 wanted, identical across two
transactions). Its rule is not known and cannot be queried without a valid proof.

Two options were weighed. Dropping the comparison for this limit and trusting the probe: cheaper,
but if the probe were wrong about a 7702 sender, the failure mode is funds stranded on the
ephemeral address. Keeping the comparison and over-provisioning: costs about 5% of the fee on
native unshields only. The second was chosen; the multiplier is a named heuristic to revisit with
more observations or with the bundler's source.

## ADR-022: predict the bundler's estimate from its source

Supersedes the heuristic of ADR-021. The bundler's figures are not measurements of the
transaction: alto bisects on fixed midpoints and scales the result, which is why the same
68,136 came back for two different transactions. Reading the rule makes the pre-proof limits land
at or above what the bundler will say, by construction, for as long as its settings hold.

What stays inferred rather than read: the two multipliers of Pimlico's public deployment (220%
call, 135% verification). Each rests on one exact match. A prediction slightly larger need is
also evaluated, because near a ladder edge our measure and alto's can fall on different steps.

This ties the single-proof path to one bundler implementation. Another bundler needs its own
policy, or falls back on the post-proof comparison to fail safely. `AltoPolicy` is a value, not
a global, for that reason.

## ADR-023: native unshield outside 4337 goes through RelayAdapt, bound in the proof

(ADR-020 in the parallel session this was ported from.)

On the 4337 transport the ephemeral 7702 sender unwraps and forwards the chain currency during
the same UserOperation. A broadcaster has no such sender: it submits the calldata it is given,
from its own EOA. The community engine solves this with RelayAdapt: the transaction unshields
the wrapped token to the RelayAdapt contract, and `relay(transactions, actionData)` runs
`unwrapBase` then `transfer` to the recipient after `transact`. `actionData` (calls, a 31-byte
salt, `requireSuccess`, `minGasLimit`) is hashed with the transactions' nullifiers into
`adaptParams`, a bound parameter of every proof, recomputed on-chain. A different recipient, a
dropped unwrap or a replay under other nullifiers all fail verification.

Reproduced exactly rather than varied, because broadcasters only accept `RelayAdapt.relay`
calldata of the shape they know, and so that the derivation can be checked against the community
ABI. The adapt params are computed inside the builder from the operations it just built, never
from a separate dummy build, so they cannot drift from what is proved.

Known property of RelayAdapt, not introduced here: with `requireSuccess = false`, a failed
`transfer` leaves the unwrapped currency on the RelayAdapt contract, where the next caller can
take it. The transfer of native currency to an address only fails if the recipient is a contract
that rejects it. The direct transport uses `requireSuccess = true`.

## ADR-032: the legacy transport's Waku node moves into the daemon

The js-waku node of the tab (ADR on the browser bridge) worked but tied the legacy transport to
an open tab and to a JavaScript bundle built from npm. `waku-light` does the same work natively
and was validated against the fleet before being wired in. It becomes the default; the tab's node
and a local nwaku stay selectable, so a regression can be worked around from the unlock form
without a new build.

The switch is a Cargo feature of the wallet, on by default. It exists for the Android build of
the Android branch (ADR-024 to ADR-031 there, not merged here), which compiles the wallet without
default features: libp2p and its TLS stack have not been built for that target yet, and its Waku
node lives in the WebView under a foreground service. Without the feature, the native mode maps
to the page's node instead of failing the unlock, which also covers a front cached from 0.12
talking to an older build. Enabling it on Android is a later, separate step.

The js-waku bundle and `/api/waku/exchange` stay in this release; they go once the native mode
has run a few days.

## ADR-033: one integration line on ZKNoxHQ, stacked in validation order

Several lines had grown from 0.10.0: the Ledger signer (Simon, `zknox/railgun-wallet`), the
native Waku node, Android (wallet as a library) and Dr Rail. They are stacked on Simon's branch
one at a time, most validated first, so that each conflict is resolved once: Waku (validated
against the fleet), then Android. Dr Rail comes last and only when its own work is at a stable
point, because it uses the wallet as a library (`railgun_wallet::db`, `keys`), which exists only
since the Android split, and because its SDK additions must follow the signer API of the Ledger
work (`spending_public_key`, async `sign`): hardware signers are the harder constraint.

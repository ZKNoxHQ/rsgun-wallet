# Changelog

## 0.13.0 (2026-09-24)

Integration release on ZKNoxHQ (`zknox/railgun-integration`): the Ledger signer work
(`zknox/railgun-wallet`, Simon), then the native Waku node (0.12.1, `zknox/railgun-wallet-waku`),
then the Android crate and the wallet library split (0.10.1, 0.11.0 to 0.11.3). No new wallet
feature. Dr Rail (`railgun-viewer`, `railgun-viewer-web`) is to be rebased onto this branch once
its current work settles.

* Features: `http` (axum server and embedded front) and `native-waku` (native Waku node), both
  on by default. The Android crate builds the library without either: no server, and the
  "native" Waku mode falls back to the WebView's node, as designed in ADR-032.
* `/api/defaults` (now in `ipc.rs`, where the Android split moved the dispatch) reports
  `nativeWaku`.
* Known risk: the wallet depends on `railgun-ledger` with its `usb` and `ble` transports enabled,
  also for the Android build. Whether coins-ledger and btleplug build for aarch64-linux-android is
  not verified; the APK workflow will tell. If not, those transports need wallet features that
  the Android crate leaves off.

## 0.12.1 (2026-09-23)

Waku mode "native", the new default: the legacy transport's Waku node runs inside the daemon
(`railgun-broadcaster::LightNodeTransport` on `waku-light`, rust-libp2p) instead of js-waku in
the wallet tab. No tab to keep open, no JavaScript on that path. Rebased onto the Ledger signer
work (`railgun-ledger`).

* Unlock form: "Native, inside the wallet" first in the Waku node list. A stored "browser" choice
  is switched to "native" once; choosing "browser" again afterwards sticks. "browser" and
  "nwaku" are unchanged and stay available as fallbacks.
* Cargo feature `native-waku`, on by default. A build without it maps "native" to "browser"
  with a warning instead of failing the unlock; `/api/defaults` reports `nativeWaku` and the
  front hides the option in that case.
* Status texts for the native mode (connecting, not ready with the node's last error).
* The front log carries the native node's own events (`waku_light`: peers connected and lost,
  filter subscriptions, dial failures) and one line per light push with the number of peers
  that accepted it.
* After a broadcaster time-out, the delivery count is reported whatever the Waku mode
  ("publishes by the native node: N accepted by a Waku peer, M failed"), from the transport's
  counters. A request no peer accepted no longer marks the broadcaster as silent, and the closing
  message points at the node rather than at the tab.
* Measured against the fleet before this release (Sepolia): the node subscribes to 2 or 3 fleet
  nodes in 5 to 30 s, and the four Railway trusted signers' rates and the offers of four
  broadcasters arrive as with the tab's node. Receiving is validated in the wallet; the first
  legacy send through the native node is still to be confirmed.
* Known: a fleet node sometimes closes the connection once, 10 to 45 s after it opened; the node
  reconnects and subscribes again within about 12 s.

## 0.10.1 (2026-09-21)

* Default gas margin of "prove once" lowered from 25% to 12%. Four live runs on Sepolia
  (two transfers, two unshields) show the paymaster validation needing 8.0 to 8.4% more than the
  gas the probe measures, a stable gap explained by the 63/64 rule; 12% keeps about 3.5% above
  it. On the transfer measured at 1,980,000 gas of limits, the same figures give about
  1,780,000, 10% less fee. The field stays editable, and a margin that turns out too low still
  stops the operation before anything is sent.
* A browser that had stored the old default (25) gets the new one; a value you typed yourself is
  kept.
* The call limit of a native unshield keeps half the margin on top of the predicted bundler
  figure (68,136 gives 80,000 at 6%).

## 0.10.0 (2026-09-20)

Native unshield (unwrap and deliver the chain currency) on the legacy and direct transports.
Ported from the work done in a parallel session, where it was numbered 0.9.7 / ADR-020: those
numbers were already taken here, hence 0.10.0 / ADR-023. If that 0.9.7 zip was applied to the
worktree, this cumulative release supersedes it.

* The transaction unshields the wrapped token to the RelayAdapt contract, which unwraps its whole
  balance and forwards the chain currency in the same EVM transaction; the submitter calls
  `RelayAdapt.relay(transactions, actionData)` (`useRelayAdapt` on the broadcaster wire). The
  recipient and the unwrap are bound into every proof through `boundParams.adaptParams`. Same
  call sequence as the Railgun community engine (`populateUnshieldBaseToken`).
* `requireSuccess` is false through a broadcaster, as in the engine (the Railgun transaction
  lands even if a call fails), and true on the direct transport, where we send it ourselves.
* SDK (`crates/railgun`): `RelayAction` (`transact::relay_adapt`) with the community's
  `adaptParams` derivation and calldata, `TransactionBuilder::relay(action)` deriving the adapt
  params from the nullifiers of the operations it just built, `ProvedTx::relay` and the
  `ProvedTx::relay` field, RelayAdapt ABI completed. Three vectors generated with the community
  ABI through ethers, plus a builder test (params bound in every operation, fee note still
  first, `adapt()` and `relay()` refused together). `railgun --lib`: 57 passed.
* 4337 is unchanged: the ephemeral sender unwraps and forwards, as before.

Not checked on a chain. On Sepolia a native legacy unshield should give a transaction whose `to`
is the RelayAdapt contract, and credit the recipient with the amount minus the 0.25% unshield
fee.

## 0.9.9 (2026-09-20)

The bundler's estimate is now predicted from its source instead of guessed.

* `userop_kit::validation_probe::alto` reproduces how alto (Pimlico) estimates: a bisection on a
  fixed ladder of midpoints (floor 9,000, allowance 30M, tolerance 10,000), then a multiplier.
  It gives the two figures observed on Sepolia to the unit: 25,312 needed for unwrap + send
  becomes 30,971 on the ladder and 68,136 at 220%; the account verification becomes 38,295 and
  51,698 at 135%. The multipliers are Pimlico's deployment settings, inferred from those matches
  (alto's defaults are 100% and 130%).
* The call limit is sized from that prediction plus half the margin, replacing the heuristic of
  0.9.8. `preVerificationGas` uses alto's way of counting (paymaster data and signature priced
  as all non-zero bytes), with the normal margin instead of twice.
* Unchanged: the comparison with the bundler's real estimate after the proof. If Pimlico changes
  its settings the prediction is off and the operation stops before sending.

## 0.9.8 (2026-09-20)

* Native unshield with "prove once" still stopped after the proof: `call needs 68136, limit is
  40000`. The probe's search works (unwrap + send: 23.6k used, 25.3k minimal limit), but the
  bundler asks for 68,136, the very same figure on two different transactions, so a rule of its
  own rather than a measurement of this call. It cannot be known before the proof.
* The call limit is now the larger of three times the searched minimum and the minimum plus 60k,
  then the margin: 100k here. This is a heuristic fitted on that one observation, not a derived
  value; the comparison with the bundler after the proof stays, and still stops the operation
  before sending if the bundler wants more.

## 0.9.7 (2026-09-20)

* Fix: "prove once" on a native unshield (unwrap and deliver) stopped after the proof with `call
  needs 68136, limit is 30000`. The call limit was sized from the gas the tail calls used
  (23.6k). Gas used is not a limit: a value transfer must have 9000 gas at hand, 34000 towards a
  new account, most of which it hands back, and every nested call keeps 1/64 in reserve.
* The probe now searches for the smallest `callGasLimit` under which the execution call succeeds:
  trials inside the same `eth_call`, each reverted so that the state stays what the paymaster
  left, doubling then bisecting to within 1000 gas. The wallet sizes the limit from that figure
  with twice the margin. The anvil test sends value to a new account and checks that the limit
  found exceeds the gas used.
* Nothing was sent and nothing was lost in the failed attempt; the token unshield of the same
  session went through with a single proof (paymaster verification 1,465,747 of 1,700,000).

## 0.9.6 (2026-09-20)

* "Empty cache" button in the header, with a confirmation that spells out the cost: full resync,
  wallet locked and to be reopened. It deletes the database of the open account on the current
  network (`POST /api/empty-cache`), waits behind a running job, and keeps
  `ephemeral_senders.jsonl`. Proofs still owed for mined transactions come back through the POI
  recovery.
* Fix of the 0.9.5 pruning rule, which left the orphans in place in the most common case: after
  a relayer that does not answer, the retry spends the same notes, so the orphan's inputs are all
  spent and it looked mined. An entry is now also dropped when its inputs were spent by an
  operation with another txid; it is kept while the spending operation is not known yet.

## 0.9.5 (2026-09-20)

* Pending POI entries of operations that never reached the chain are dropped at sync (SDK fork:
  `PoiProvider::prune_unmined`). Rule: a mined operation nullifies all its inputs, so an entry
  with an input still unspent, 15 minutes or more after the proof, was not sent. Entries written
  before the timestamp existed count as old, which clears the orphans left by the legacy
  time-outs of versions before 0.9.2. The clean-up of 0.9.2 only covered the operation that had
  just timed out.
* Nothing to clean on the POI node: an entry is only submitted once its txid is validated in the
  txid tree, so an unmined operation never left the wallet.
* The front hint under the pending list says so.

## 0.9.4 (2026-09-20)

Legacy requests going unanswered. No change since the validated 0.7.x touches the sealing,
publishing or response path, so this release makes the failure observable and removes the two
weaknesses that fit the symptom.

* Every publish is acknowledged by the tab (`acks` in `/api/waku/exchange`): on a time-out the
  job now says how many publishes a Waku peer accepted, how many failed and why, and how many the
  tab never acknowledged. A request that never left the tab and a broadcaster that stays silent
  are no longer the same message, and the advice differs (reload the page, or use another
  broadcaster).
* Broadcaster selection follows the reference client: a draw among the offers within 10% of the
  cheapest, instead of always the cheapest, which sent every request to the same broadcaster
  (reliability 0.62 on Sepolia). A broadcaster that stayed silent although the request was
  delivered is left out of the draw for 10 minutes.
* A broadcaster can be chosen by hand in the offers table ("use"), to test each one.

## 0.9.3 (2026-09-20)

* The wallet version is shown in the header and the page title (`version` in `/api/defaults`).
* The public account is derived from the recovery phrase when no key is given: BIP-44
  `m/44'/60'/0'/0/<index>`, same index as the Railgun keys, checked against the hardhat phrase
  (accounts 0 and 1). Its address, balance and derivation path are shown; an imported key still
  takes precedence and is labelled as such. With raw Railgun keys and no phrase there is no
  public account, as before.

## 0.9.2 (2026-09-20)

* Legacy: a broadcaster that does not answer within 120 s no longer ends the job on "may have
  been sent". The wallet syncs up to four times over 80 s and looks at the input notes: spent
  means mined (answer lost), still unspent means not sent. In the second case the pending POI
  entries of the proof are dropped (SDK fork: `RailgunProvider::discard_pending_poi`), since
  their txid will never be validated; `recover_missing` rebuilds them if the transaction lands
  later after all.
* "Prove once" validated on Sepolia with the simulated limits (0.9.1). Calibration from that
  run: the probe measured 1,393,206 gas of paymaster validation, the estimate on the real proof
  asked for a limit of 1,507,574 (+8.2%, the 63/64 forwarding rule over four nested calls), the
  25% margin allowed 1,750,000.

## 0.9.1 (2026-09-20)

* Fix: "prove once" stopped after the proof with `account verification needs 51698, limit is
  20000`. The probe measures the account's `validateUserOp` (9.5k), but EntryPoint v0.8 charges
  its own pre-validation work to `verificationGasLimit` as well (`AA26`): copying the
  UserOperation, prefund, the EIP-7702 sender path, nonce validation. The probe replaces the
  EntryPoint and cannot measure that. The limit is now the account's gas plus
  `ENTRY_POINT_VALIDATION_OVERHEAD` (60k), then the margin.
* First live run of the probe (Sepolia): the public RPC accepts state overrides, the real
  paymaster validation was simulated before any signature (1,393,206 gas), and the
  pre-verification and paymaster limits derived from it passed the checks on the real proof.

## 0.9.0 (2026-09-20)

"Prove once" no longer depends on anything learned or configured.

* The gas profiles of 0.8.0 are gone (`gas_profiles.json`, learning from the iterative path,
  keys per shape and per chain). They were a cache of measurements that could go stale with a
  contract upgrade, and the first transaction of each shape still signed several times.
* Instead the exact UserOperation is simulated before any signature, with a dummy proof:
  `userop_kit::validation_probe` builds an `eth_call` sent from the Railgun verification bypass
  origin, with a small probe contract overriding the EntryPoint's code. Account validation,
  paymaster validation (which performs the `transact`), tail calls and `postOp` run in EntryPoint
  order and the probe reports the gas of each. The fresh 7702 sender gets the code of its
  implementation by override. Two rounds, then the margin (default 25%, 10k gas steps).
* Only `preVerificationGas` is computed, by the ERC-4337 reference formula plus the 7702
  authorization cost: it prices inclusion, not execution. It gets twice the margin, and the
  bundler's estimate on the final UserOperation is still checked before sending.
* Strict mode ("never fall back"): when the limits cannot be simulated, typically an RPC without
  state overrides, stop before any signature instead of using the iterative path.
* SDK fork: `RailgunProvider::dummy_userop`, `prepare_userop_single_proof` now takes the gas
  figures; `UserOpGasProfile`, `dummy_transact_gas` and the profile parameters are removed.

Checked here: the probe against anvil with mock account and paymaster (caller seen as the
EntryPoint, bypass origin, phases in order with state carried over, revert reasons, nothing
persisted), unit tests, API and front. Not checked: the probe against the real paymaster and a
7702 sender, and whether the public RPC accepts state overrides.

## 0.8.1 (2026-09-20)

Documentation only. The 4337 "prove once" path is validated on Sepolia: a transfer in the default
mode recorded the gas profile of its shape, the same transfer with "prove once" then went through
with a single proof, limits accepted by the paymaster and the bundler. The margin (25%) has not
been tuned against the measured figures yet.

## 0.8.0 (2026-09-20)

Aimed at hardware and threshold signers, where every spending signature is a confirmation on the
device or a signing ceremony.

* Fix (SDK fork): `build_dummy` no longer reaches the signer. It went through
  `TransactCircuitInputs::from_inputs`, which signs; invisible with a software key, one
  confirmation per gas estimate with a Ledger. New `from_inputs_unsigned`, and a unit test with a
  counting signer. The legacy path now costs one signature per operation instead of three.
* 4337 "prove once" option (SDK fork: `RailgunProvider::prepare_userop_single_proof`): gas
  limits are fixed before proving, from the dummy-proof gas of the `transact` call, a
  `UserOpGasProfile` learned on an earlier operation of the same shape, and a margin (default
  25%, limits rounded up to 10k gas). The fee follows from the limits, so the paymaster check
  passes by construction. After proving, the paymaster verification gas is measured on the real
  proof and the bundler's own estimate is compared with the limits; if one is too low the
  operation stops before anything is sent.
* Profiles are learned from every converged iterative estimate and kept in
  `<data-dir>/<chain>/gas_profiles.json`, keyed by shape (`1x2+2x3|calls=0`). Without a profile
  for a shape, "prove once" falls back to the iterative path for that transaction and says so.
* userop-kit fork: `Bundler::gas_price`, the bundler's price without a simulation
  (`pimlico_getUserOperationGasPrice`).
* If the dummy-proof estimate fails in the default mode, the operation carries on without
  learning: the path validated on Sepolia is unchanged.

Checked here: unit tests (54 in `railgun`), API and front. Not checked: the single-proof path on
a chain. It needs a learned profile, so the first live run is: one 4337 transfer in the default
mode (records the profile), then the same with "prove once".

## 0.7.3 (2026-09-20)

Documentation only. Legacy transport validated end to end on Sepolia: private transfer and
token unshield relayed by a community broadcaster, pre-transaction POIs accepted, and
post-transaction POIs validated (change notes spendable again after sync). README and ADR-009
no longer list these as unverified. Still open: native unshield over legacy (RelayAdapt),
mainnet.

## 0.7.2 (2026-09-20)

* Fix: the legacy gas estimate reverted with "RailgunSmartWallet: Gas price too low". The
  dummy-proof transaction binds `minGasPrice`, and an `eth_estimateGas` that names no price runs
  with `tx.gasprice = 0`. The estimate now carries `gasPrice = minGasPrice`, as the type 0
  transaction the broadcaster sends. Found on the first live run (Sepolia); the offline tests
  could not reach this step, which needs shielded notes and a real node.
* If the node then answers "insufficient funds" (it caps the gas by the balance of the bypass
  address once a price is named), the estimate is retried with a balance override.

## 0.7.1 (2026-09-20)

* Fix: every legacy operation failed with "no usable broadcaster offer" on the real network.
  The wallet passed its POI list keys as `ListKey(…)` (the type's `Display` is a debug form), so
  no broadcaster's `requiredPOIListKeys` ever matched. `RailgunProvider::poi_list_keys` now
  returns the wire form. The mock broadcaster required no list, which is why tests missed it;
  it can now require lists and a regression test covers the case.
* The front applies the same POI list filter as the engine (`poiListKeys` in the status), so it
  no longer counts as usable an offer the engine will refuse.
* A mismatch of POI lists has its own error, naming the lists required and the wallet's.

First run against the live Railgun fleet (Sepolia, 2026-09-20): the js-waku node in the tab
connected to 3 peers, received offers, and the four Railway trusted signers do announce a rate
there.

## 0.7.0 (2026-09-20)

* The Waku node of the legacy transport runs in the wallet tab by default: js-waku light node
  (`@waku/sdk` 0.0.36, as the reference web client), filter to receive, light push to send,
  dialling the three fixed wss peers of the Railgun fleet. No nwaku, no docker. The previous mode
  stays available ("Local nwaku over REST" at unlock).
* `static/waku-bundle.js` (832 kB, no WebAssembly, no eval) is embedded in the binary and served
  at `/waku-bundle.js`; sources and build command are in `waku-bridge/`.
* `POST /api/waku/exchange`: once a second the tab hands over what it received and takes what
  it must publish. The daemon reports the transport unreachable when the tab stops calling
  (8 s) or while its node is not connected, with the node's own state in the message.
* CSP: `script-src` gains `'self'`, `connect-src` gains `wss://*.rootedinprivacy.com:8000` and
  nothing else, since there is no DNS discovery.
* Diagnostics of the legacy tab describe the state of the tab's node (loading, dialling,
  subscribing, connected, error with retry every 30 s).

Checked here: bundle loads under the CSP without violation, the node starts and dials the three
fleet peers (blocked by this environment's network, so the time-out and retry path is what ran),
the daemon sees the tab's state, offers carried through `/api/waku/exchange` are authenticated
and listed, the transport goes unreachable when the tab disappears. Not checked: a real
connection to the fleet.

## 0.6.0 (2026-09-20)

* Fee ceilings on the legacy transport, independent of any trusted signer:
  * highest rate accepted, as a multiple of the gas cost (default 1.5). Offers above it are
    ignored at selection; if none is left the operation fails before anything is proved, with
    the cheapest rate refused.
  * optional highest fee for the whole transaction, in wrapped token, checked after the gas
    estimate and before proving.
  Both are sent with each operation (`maxFeeRate`, `maxFee`) and remembered by the front.
* Rates are shown as multiples of the gas cost instead of raw per-1e18 values; offers over the
  ceiling are tagged in the table and no longer count as usable.
* The fee in the job steps is shown in token units.
* Before this version a legacy operation went from quote to broadcast with no bound on the fee
  other than the shielded balance when no trusted signer was set.

## 0.5.2 (2026-09-20)

* Transport tabs are always selectable. An unusable transport is marked "(unavailable)" and,
  once selected, says why and what to do; only the Transfer and Unshield buttons are disabled.
  Before, the legacy tab was simply greyed out with the reason hidden in a tooltip.
* Legacy diagnostics: no Waku node at the URL (with the `fake_network` command), node without
  peers, trusted signers that have not announced a rate, offers outside the band, no offer yet.

## 0.5.1 (2026-09-20)

* Trusted fee signers prefilled with the four signers of the Railway wallet
  (`railgun_broadcaster::RAILWAY_TRUSTED_FEE_SIGNERS`, served by `GET /api/defaults`).
* "none" in the field disables the cap, for test networks where these signers may not announce.

## 0.5.0 (2026-09-19)

* Trusted fee signers for the legacy transport (`railgun-broadcaster` 0.2.0): field at unlock
  (one or several 0zk addresses), saved with the other non-secret preferences.
* The legacy panel shows the number of signers, the authorized rate and the accepted range, or
  says that offers are held back until a signer announces.
* Mainnet refuses the legacy transport without a trusted signer. Sepolia works without.
* A malformed signer address fails the unlock with an explicit message.

## 0.4.0 (2026-09-19)

* Legacy transport through the new `railgun-broadcaster` crate. The transport is chosen per
  operation between 4337, legacy and direct.
* Unlock takes a Waku node REST URL (default `http://127.0.0.1:8645`). A monitor task keeps the
  broadcaster offers current; `/api/status` returns the live state under `legacy`.
* The legacy tab turns on when at least one usable offer exists for the wrapped base token, and
  lists the offers. When the node is unreachable the reason is shown.
* `submit_legacy`: best quote, gas price from the node plus 10%, two dummy-proof gas estimates,
  fee note pinned first, `minGasPrice` bound, proof, pre-transaction POIs, sealed request, wait
  for the transaction hash. A timeout says to sync and check the inputs before retrying.
* Native unshield is refused over legacy (needs RelayAdapt).
* Fork: `RailgunProvider::poi_list_keys`.

Checked here: unit and end-to-end tests of the crate, wallet against the `fake_network` example
over real HTTP (subscription, offers authenticated, tab and table in the front, flow up to
`eth_gasPrice`). Not checked: proving on this path and a live broadcaster.

## 0.3.0 (2026-09-19)

SDK prerequisites for the legacy (broadcaster) transport. No transport code yet, the legacy tab
stays disabled. All in the `crates/railgun` fork, all opt-in:

* `TransactionBuilder::min_gas_price(wei)`: sets `BoundParams.minGasPrice` (was hardcoded 0).
  Default 0, so existing proofs are bit-identical.
* `TransactionBuilder::broadcaster_fee(from, to, asset, value)`: a transfer whose note is
  guaranteed to be `transactions[0].commitments[0]`. Refused if it would span two UTXO trees or
  if one is already set. Without it the layout is unchanged.
* `RailgunProvider::build_dummy`: same calldata with an all-zero proof, for `eth_estimateGas`
  from `VERIFICATION_BYPASS` (0x…dEaD). No proving, no POI registration.
* `RailgunProvider::pre_transaction_pois`: pre-transaction POI per list key and txid leaf hash
  (pre-inclusion UTXO position 199999/199999, dummy txid proof with zero siblings).
  `PoiCircuitInputs::from_inputs_with_txid_proof`, `TxidLeafHash::dummy_proof`.
* `crypto::aes`, `SharedKey` and `ViewingKey::derive_shared_key` are public, for the
  wallet-to-broadcaster ECDH channel.
* Wallet: the `direct` transport logs a dummy-proof gas estimate before proving, to exercise
  the new path on a live chain.

Tests: 6 new unit tests (default layout unchanged, fee pinned first including across groups and
trees, fee rules, minGasPrice bound and zero proof, dummy txid proof). `cargo test -p railgun
--lib`: 51 passed. `railgun-ts` and the wallet compile against the fork.

## 0.2.0 (2026-09-19)

* POI recovery for operations this process did not build (wallet restored from its keys,
  transactions made with another wallet). Change notes stuck in "waiting for proof" now get
  their proof rebuilt and submitted during sync. Implemented in the `crates/railgun` fork:
  * `IndexedAccount` keeps spent notes and decrypts, as sender, the outputs addressed to other
    wallets (`note/sent.rs`).
  * `TxidIndexer` retains the full record of operations spending one of our nullifiers;
    `Operation` carries `has_unshield` (subsquid `hasUnshield`).
  * `PoiProvider::recover_missing` rebuilds a `PendingPoiEntry` per past operation whose outputs
    the POI node reports as `Missing`, oldest first.
* Database directory moves to `db-v2`: a v1 database discarded the data above, the first sync
  after the upgrade is a full one.

## 0.1.2 (2026-09-19)

* Default endpoints: public RPC per network (publicnode) and the public Pimlico bundler,
  prefilled in the front and applied by the daemon when the field is empty. Switching network
  swaps the defaults and leaves custom values alone.
* Recovery phrase prefilled with the public test phrase ("yellow" x12), flagged as test only.
* Empty or scheme-less RPC URL now gives an explicit error instead of `relative URL without a
  base`.

## 0.1.1 (2026-09-19)

* `WalletDb` replaces `kohaku_db::fs::FilesystemDatabase`: file name is `sha256(key)`, writes
  are atomic (temp file then rename), directory mode 0700. Fixes `File name too long (os error
  36)` raised by `RailgunProvider::register` on any filesystem-backed database.
* Balances and EOA balance shown without trailing zeros.
* Verified: unit tests (key derivation vector from the Railgun engine, long-key database),
  daemon smoke test against a mock RPC (unlock, register, snapshot, origin/host refusal, legacy
  refusal, sync failure surfaced), front driven with Chromium against the real daemon.

## 0.1.0 (2026-09-19)

* New crate `railgun-wallet`: loopback daemon (axum) and embedded front.
* Engine actor on a dedicated thread owning `RailgunProvider`; job queue, step log, SDK log
  mirrored to the front.
* Operations: sync, shield (ERC-20 with approval, native through RelayAdapt), private transfer,
  unshield (token, or native with unwrap over 4337).
* Transports: `erc4337` (privacy paymaster), `direct` (debug), `legacy` declared and disabled.
* Mnemonic derivation: Railgun engine scheme and kohaku BIP-32 scheme.
* POI: enabled through `RailgunBuilder::with_poi`, per-note status and pending submissions shown.
* Fork patch in `crates/railgun`: read-only accessors `RailgunProvider::{chain, synced_block,
  poi_enabled, poi_pending}` and `PoiProvider::pending_summaries`.

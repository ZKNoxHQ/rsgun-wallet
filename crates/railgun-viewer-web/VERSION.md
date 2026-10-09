# railgun-viewer-web — journal des versions

## 0.1.4 — 2026-09-25

- Avertissement du viewer 0.2.20 quand le réseau n'atteint pas le nœud POI (cas des réseaux mobiles IPv6 seuls avec ppoi.fdi.network), après l'unlock et dans la carte POI de l'onglet Network.

## 0.1.3 — 2026-09-24

- Nœud Waku Rust dans le Web Worker : `LightNodeTransport` (feature `light-node` du broadcaster 0.7.0, waku-light 0.4.1 sur le WebSocket du navigateur), un client par chaîne gardé entre les vérifications. `/api/defaults` répond `nativeWaku: true` : l'onglet Network propose le nœud Rust par défaut, comme le daemon, et js-waku dans la page reste en secours. Même code de sonde que le viewer 0.2.19 (attente de l'abonnement, fenêtre tolérante aux trous).

## 0.1.2 — 2026-09-24

- Suit le viewer 0.2.18 (fichiers partagés) : `health::run(p, bridge, None)`, `/api/defaults` avec `nativeWaku: false`, donc le front garde le nœud js-waku de la page. Le nœud Rust dans le worker viendra à l'étape suivante.

## 0.1.1 — 2026-09-24

- Rebase sur `zknox/railgun-integration` avec le viewer 0.2.17 : `signer.rs` (inclus par `#[path]`) implémente le trait async de la base, dépendance `async-trait` ajoutée. `railgun-broadcaster` 0.7.0 de la base (wasm natif) à la place de la 0.6.2.
- Workflow `dr-rail-web.yml` déclenché sur `zknox/dr-rail`. `dist/` ignoré par git.

## 0.1.0 — 2026-09-23

Première version : Dr Rail en site statique, sans serveur.

### Ajouté
- Crate `railgun-viewer-web` (cdylib wasm-bindgen) sur les sources du viewer 0.2.16 incluses par `#[path]` et `railgun-wallet/src/keys.rs` ; point d'entrée unique `api(method, path, body)` servant les mêmes routes que le daemon (`status`, `snapshot`, `unlock`, `sync`, `refresh`, `lock`, `derive`, `health`, `health/run`, `defaults`, `waku/exchange`).
- Acteur de commandes identique au daemon (unlock répond après le contrôle des clés puis poursuit, sync, refresh, lock), dans une tâche `spawn_local`.
- Base du SDK (`JsDatabase`) et cache du viewer en IndexedDB (`js/idb.js`), base `drrail-<chaîne>-<sha256(adresse)[..8]>`.
- Web Worker (`web/worker.js`) hébergeant le wasm, routeur `fetch` du front (`web/shim.js`), page générée depuis le front du viewer (`web/make_index.py` : shim en tête, chemin relatif du bundle Waku, mention d'exécution locale).
- `build.sh` (cargo, wasm-bindgen, wasm-opt si présent) et workflow `.github/workflows/dr-rail-web.yml` (GitHub Pages + artefact `dr-rail-web`).

### Vérifié
- Préflight CORS (origine tierce, POST, `content-type`) acceptés par ppoi.fdi.network, le subsquid Sepolia et publicnode (`access-control-allow-origin: *`).
- `cargo check --target wasm32-unknown-unknown -p railgun-viewer-web` sans erreur ni warning ; build release et wasm-bindgen 0.2.108.

### Non validé
- Parcours complet dans un navigateur (unlock, scan, POI, onglet Network).

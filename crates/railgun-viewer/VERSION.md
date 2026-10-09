# railgun-viewer — journal des versions

## 0.2.20 — 2026-09-25

### Ajouté
- Nœud POI injoignable depuis le réseau : après l'unlock (POI actif), un appel `ppoi_validated_txid` sans réponse HTTP (refus, DNS, timeout, CORS) laisse une ligne `✗` dans le Log et l'erreur courante, au lieu de statuts « inconnu » sans explication. Pour le nœud par défaut, le message explique que ppoi.fdi.network publie une IPv6 qui refuse les connexions : les réseaux avec IPv4 (Wi-Fi) retombent dessus, les réseaux mobiles IPv6 seuls non. Même texte dans la carte POI de l'onglet Network (`unreachable from this network`). Une erreur JSON-RPC du nœud compte comme une réponse. `health::poi_reachable`, `health::POI_IPV6_HINT`, `session::warn_if_poi_unreachable`, communs au daemon et à la version web.

## 0.2.19 — 2026-09-24

### Corrigé
- Sonde des broadcasters par le nœud Rust en `down` immédiat (« native Waku node not ready: 0 peer(s)… », 1 ms) : le premier `subscribe` démarre le nœud et répond « pas prêt » tant qu'il n'a pas composé la flotte ni ouvert ses abonnements filter. Il est maintenant relancé chaque seconde pendant une minute au plus (l'ajout de topics est idempotent). Pendant la fenêtre d'écoute, une erreur de `poll` du nœud Rust (pair de la flotte perdu, reconnexion automatique) ne clôt plus la fenêtre. Le détail de la carte indique la source (`native`, `tab`) au lieu de `tab` pour tout ce qui n'était pas nwaku.

## 0.2.18 — 2026-09-24

### Ajouté
- Onglet Network : sonde des broadcasters par le nœud Waku Rust du viewer (`LightNodeTransport`, feature `light-node` du broadcaster 0.7.0, même nœud que le mode natif du wallet 0.13), choix par défaut. Le nœud démarre à la première vérification et reste connecté (un client par chaîne dans `AppState::native_clients`) ; attente des pairs jusqu'à une minute comme pour l'onglet ; niveau `down` s'il n'en trouve aucun.
- Sélecteur « Waku node » : « in the viewer (Rust) » ou « this tab (js-waku) » ; le champ nwaku REST reste prioritaire quand il est rempli. Le nœud js-waku de la page ne démarre plus qu'en mode onglet (ouverture de l'onglet, changement de réseau, changement de source).
- `/api/defaults` expose `nativeWaku` (daemon : oui) ; sans lui, le front retire le choix Rust et revient à l'onglet.
- `HealthParams::waku_source` (`native` | `tab` | `nwaku`) ; absent : nwaku si une URL est donnée, sinon le nœud Rust s'il existe, sinon l'onglet. `health::run` prend les deux clients (onglet, natif). Voir ADR-013.

## 0.2.17 — 2026-09-24

Rebase sur `zknox/railgun-integration` (ZKNoxHQ/kohaku : signer Ledger de Simon, Waku Rust, Android, wallet 0.13.0), branche `zknox/dr-rail`. Aucun changement de comportement.

### Modifié
- `RailgunSigner` de la base : `spending_public_key()` obligatoire, `sign` async (`async-trait`, `?Send` en wasm), plus de `spending_key()` dans le trait. `MasterSigner` suit : clé publique de dépense factice (seul le master key sert aux chemins de lecture) et `async fn sign` qui refuse ; dépendance `async-trait` ajoutée.
- Ajouts SDK du viewer réappliqués sur les fichiers de Simon (fusion à trois voies depuis `fb331ef`) : `spending_pubkey()` supprimée au profit de `spending_public_key()` (`utxo_indexer`, `note/utxo`, `poi/provider`) ; `master_public_key()` par défaut construit depuis `spending_public_key()` et le nullifying key, `address()` passe par elle ; `RailgunSignerError::new` est celle de la base (toute erreur, chaînes comprises). `builder.rs`, `chain_config.rs`, `provider.rs`, `indexer/syncer/mod.rs` sans conflit.
- Membres du workspace : `crates/railgun-viewer`, `crates/railgun-viewer-web`.
- Écartés du snapshot : broadcaster 0.6.2 (la 0.7.0 de la base couvre wasm avec la même API), overlays waku-light 0.1.x et wallet 0.12.0, `static/index.html.bak-*`.

## 0.2.16 — 2026-09-23

### Modifié (sans changement de comportement)
- Cœur de session sorti de `engine.rs` dans `session.rs` : `UnlockParams` (avec `precheck`, `lengths`, `rpc_url`), résolution du signer (`make_signer` : mnémonique, adresse 0zk, découverte on-chain), `Session`, `build_history`, `note_in` / `sent_in`, cache du viewer (`Caches`, persistance par le trait `CacheStore`, fichier `viewer-cache.json` côté daemon). `engine.rs` ne garde que l'acteur, le runtime, `WalletDb` et le fichier de cache.
- `keys.rs` : dérivation via `crate::wallet_keys` (le fichier du wallet, ré-exporté par `main.rs`), `parse_address` / `master_from_address` / `non_empty` publics, `derive_display` (corps de `/api/derive`). Adresse 0zk refusée si elle ne fait pas 127 caractères avant tout parsing.
- `waku_link.rs` : `/api/defaults` et `/api/waku/exchange` (liaison avec le nœud js-waku de la page), `shared::status_view` pour `/api/status`.
- `shared::now_ms` et `health.rs` (horloge `web_time`, pause `gloo-timers`, client HTTP sans timeout global) compilent aussi pour wasm32.

Ces fichiers sont compilés tels quels par `crates/railgun-viewer-web` (version web statique). Voir ADR-012.

## 0.2.15 — 2026-09-23

### Corrigé
- Plus de moyen de saisir la clé de visualisation et l'adresse après la 0.2.14 : le correctif CSS rendait enfin l'attribut `hidden` effectif, ce qui a révélé que le bloc de la clé n'était jamais redécouvert par le gestionnaire du sélecteur (masqué jusque-là par `.vw-key{display:grid}`). La visibilité des blocs est désormais pilotée par la valeur du sélecteur, indépendamment du gestionnaire de la page : changement, bascule programmatique au changement de réseau, attribut `hidden` ou style réécrits par la page. `!important` retiré de la règle CSS.

### Modifié
- Sélecteur « Key source » à trois entrées : « Mnemonic » (Sepolia), « Private viewing key + 0zk address » (adresse obligatoire, master public key lu dans l'adresse après contrôle de la clé publique de visualisation, aucun scan) et « Private viewing key only » (découverte on-chain, aucune adresse envoyée). Le champ adresse n'apparaît que pour la deuxième. Si le sélecteur reste sur « Mnemonic » avec la clé et l'adresse remplies, `Load` prend la deuxième.
- Texte sous la clé selon la source choisie.
- DECISIONS.md : amendement de l'ADR de la 0.2.14 (source de clé à part entière).

### Livraison
- `static/index.html` et `DECISIONS.md` modifiés par `patch_front.py` (non extrait), applicable sur le front 0.2.13 ou 0.2.14 ; sauvegarde `index.html.bak-<version>`. Daemon inchangé (champ `address` de `POST /api/unlock` depuis la 0.2.14).

## 0.2.14 — 2026-09-23

### Ajouté
- Adresse 0zk facultative avec la clé de visualisation privée (champ `vwAddress` sous la clé, corps `address` de `POST /api/unlock`, alias `zkAddress` / `railgunAddress`). Si elle est fournie, le master public key en est lu après contrôle que sa clé publique de visualisation est celle de la clé saisie (refus immédiat sinon), sans parcours de la chaîne. Sans adresse, la découverte on-chain de la 0.2.7 reste le repli, et son message d'échec renvoie vers l'adresse. Voir DECISIONS.md (adresse 0zk facultative, amende ADR-010).
- Parsing d'adresse protégé (`catch_unwind`) : `RailgunAddress::from_str` découpe le payload bech32 sans contrôle de longueur et pouvait faire tomber le thread du moteur.

### Corrigé
- Cause réelle du refus « provide a mnemonic or a private viewing key » (journal `mnemonic=0 viewingKey=0`) : la règle `.vw-key{display:grid}` l'emportait sur l'attribut `hidden`, les deux blocs de saisie étaient visibles, le sélecteur restait sur « Mnemonic » et le front envoyait une mnémonique vide. Ajout de `.vw-key[hidden]{display:none}` ; `creds()` prend ce qui est rempli quel que soit le sélecteur, message clair si tout est vide, contrôle de forme de l'adresse côté client.

### Livraison
- `static/index.html` et `DECISIONS.md` modifiés par `patch_front.py` (fourni dans le zip, non extrait), faute de disposer de l'état 0.2.13 de ces fichiers ; sauvegarde `index.html.bak-0.2.13`.

### Non validé
- Compilation sur la machine cible.
- Durcissement de la découverte on-chain (candidat validé par une note dépensée, chemin receveur seul) : nécessite `note/utxo.rs` et `builder.rs` du fork.

## 0.2.13 — 2026-09-23

### Corrigé
- « provide a mnemonic or a private viewing key » avec le champ rempli : le corps de `POST /api/unlock` était désérialisé avec `serde(flatten)` pour les identifiants ; la structure est maintenant plate (`mnemonic`, `index`, `derivation`, `viewingKey` lus directement, alias `viewing_key` / `viewingPrivateKey` / `key` acceptés), et en cas de refus le journal indique les longueurs reçues pour lever le doute entre front et daemon.

## 0.2.11 — 2026-09-23

### Modifié
- Logo agrandi (100×150 affiché, vignette WebP 200×300 embarquée).
- Mnémonique saisie : la clé de visualisation privée dérivée s'affiche sous le champ (avec l'adresse 0zk du réseau choisi et un bouton Copy), recalculée à la frappe via `POST /api/derive` (dérivation pure, sans moteur ; schéma et index pris en compte). Masquée après chargement.

## 0.2.10 — 2026-09-23

### Modifié
- Plus de mention ZKNOX dans l'interface (titre de page « Dr Rail »).
- Mnémonique réservée à Sepolia : champ grisé et vidé, option « Mnemonic » désactivée et bascule automatique sur « Private viewing key only » dès qu'un autre réseau est choisi ; refus côté client si forcé. Mention sous le champ : « Sepolia only, for tests, never divulgate a real seed phrase here ».

## 0.2.9 — 2026-09-23

### Modifié
- Titre de l'interface « Dr Rail », illustration fournie (chat en blouse) en logo à gauche de l'en-tête, à la place du « R » ; vignette WebP 107×160 embarquée en data URI (7 Ko), pas de fichier externe.

## 0.2.8 — 2026-09-23

### Corrigé
- Subsquid : ce squid n'a pas de filtre `_in` sur les champs `Bytes` (`transactionHash_in`, `nullifier_in` refusés par le schéma, seuls `_eq` / `_not_eq` / `_isNull` existent). Les events `unshields` et `nullifiers` sont maintenant demandés par bloc (`blockNumber_in`, BigInt) et rapprochés localement par hash de transaction et par nullifier. Les commitments restent demandés par `hash_in` (BigInt).

## 0.2.7 — 2026-09-23

### Modifié
- Sources de clés réduites à deux : mnémonique, ou clé de visualisation privée seule. Les saisies « adresse 0zk » et « clé partageable (msgpack) » sont retirées de l'interface et du code (`SpubSigner`, dépaquetage BabyJubJub, `rmp-serde`, `ark-*` supprimés).
- Clé de visualisation seule : le master public key est retrouvé sur la chaîne avant l'enregistrement du compte (`RailgunBuilder::discover_master_key`, scan complet des commitments au premier chargement, progression dans le journal), puis le compte est enregistré avec `MasterSigner`. Refus explicite si aucune note de transfert reçue en clair n'existe (compte n'ayant reçu que des shields, ou expéditeurs révélés).

### Modifié (crates/railgun, fork ZKNOX)
- `note::utxo::discover_master_key(viewing_key, transact)` : déchiffre une note de transfert avec la seule viewing key, lit le master key du receveur en clair dans le premier champ du bundle et ne le retient que s'il reproduit le hash de commitment on-chain.
- `RailgunBuilder::discover_master_key(viewing_key, from_block, progress)` : parcours des commitments avec la même chaîne de syncers que `build()` (subsquid puis RPC), par tranches de 50 000 blocs, retour au premier match avec le timestamp de bloc de la note.

## 0.2.6 — 2026-09-23

### Modifié (UX)
- Attente du nœud Waku de l'onglet ramenée à 30 s (front et daemon).
- Niveaux broadcasters : `down` si connecté à des pairs et aucun signataire après la fenêtre d'écoute (panne réseau, message d'erreur explicite), `warn` si des broadcasters existent mais aucune offre utilisable pour le jeton de base, `skip` seulement quand le nœud de l'onglet n'a trouvé aucun pair (problème local).
- Changement de réseau : le rapport précédent est effacé immédiatement (cartes vides, pastille neutre, « network changed: run the checks again ») ; un rapport reçu pour une autre chaîne que celle sélectionnée n'est jamais affiché.

## 0.2.5 — 2026-09-23

### Modifié (UX)
- Subsquid : seuils exprimés en temps, pas en blocs (95 blocs sur Arbitrum font 24 s, pas un retard). Retard = blocs × durée de bloc de la chaîne (12 s mainnet et Sepolia, 1,5 s BNB, 2 s Polygon, 0,25 s Arbitrum) ; ok ≤ 5 min, warn ≤ 30 min, down au-delà ; résumé « n block(s) ≈ 24 s behind ».
- Broadcasters au premier clic : `Run checks` attend d'abord que le nœud Waku de l'onglet soit `connected` (jusqu'à 60 s, état affiché), et le daemon attend lui aussi des pairs avant d'ouvrir la fenêtre d'écoute (`waitedForPeersSecs` dans le détail). Le bouton se réactive en cas d'erreur.

## 0.2.4 — 2026-09-23

### Corrigé
- Le nœud Waku de l'onglet restait abonné aux topics du premier réseau choisi : après un passage à mainnet il n'entendait que Sepolia (3 pairs, 0 message). Il est maintenant redémarré au changement de réseau (et arrêté si une URL nwaku est saisie), et `Run checks` s'assure d'abord qu'il tourne pour la chaîne sélectionnée, avec un avertissement dans le journal s'il n'est pas encore connecté.
- Ligne d'état du nœud : chaîne affichée.
- Écoute par défaut portée à 15 s (max 90).
- Warning `scheme is never read` : le schéma de dérivation est journalisé au déverrouillage.

## 0.2.3 — 2026-09-23

### Ajouté
- Broadcasters sans nwaku : l'onglet Network fait tourner le même nœud Waku léger que le wallet (js-waku, bundle `crates/railgun-wallet/static/waku-bundle.js` embarqué et servi sur `/waku-bundle.js`), relié au daemon par le `BrowserBridge` de `railgun-broadcaster` (`GET /api/defaults`, `POST /api/waku/exchange`, un aller-retour par seconde). La sonde broadcasters utilise ce lien quand le champ nwaku est vide ; un `BroadcasterClient` par chaîne est créé sur le bridge à la première demande. Ligne d'état du nœud de l'onglet (loading, connecting, subscribing, connected · n peers, error).
- Niveau `skip` tant que le nœud de l'onglet n'est pas connecté (jusqu'à une minute après ouverture de l'onglet), `warn` connecté sans broadcaster ou sans offre utilisable, `ok` sinon ; le détail indique la source (`tab` ou `nwaku`).

### Dépendances
- `base64` 0.22 (encodage des messages du bridge).

## 0.2.2 — 2026-09-23

### Corrigé
- Subsquid déclaré « down » à tort : `squidStatus.height` est la hauteur finalisée de ce squid (des entités existent au-delà). La tête indexée est maintenant le maximum entre cette hauteur, le bloc de la dernière opération et celui du dernier commitment ; le retard est mesuré dessus, `finalizedHeight` est lu quand il existe, et les seuils passent à warn > 50 blocs, down > 500 (sur la tête indexée et, si connue, la hauteur finalisée).
- Nœud POI : `ppoi_node_status_v2` tenté sans puis avec les paramètres de chaîne, l'erreur est conservée dans le détail (`nodeStatusError`) au lieu d'un `null` muet ; réponse brute de `ppoi_validated_txid` conservée (`validatedRaw`), lecture du merkleroot sous plusieurs noms de champ.
- Broadcasters : un nœud nwaku local injoignable donne `skip` avec un message explicite, pas `down` (ce n'est pas un verdict sur le réseau).

## 0.2.1 — 2026-09-23

### Corrigé
- `NwakuRest` importé depuis la racine de `railgun-broadcaster` (ré-export), pas depuis `transport`.

### Ajouté
- Réseaux BNB Chain (56), Polygon (137) et Arbitrum One (42161) dans le SDK (`ChainConfig::{bnb, polygon, arbitrum}`, `from_chain_id`, `ChainConfig::all()`), dans le sélecteur du viewer et dans les RPC publics par défaut (publicnode). Proxy Railgun, jeton enveloppé et subsquid suivent shared-models ; les adresses de relay-adapt et les blocs de départ POI (champ non utilisé par le SDK) sont à vérifier avant tout envoi relayé sur ces chaînes ; paymaster et fee adapter absents (pas de flux 4337 sur ces chaînes dans le fork).

## 0.2.0 — 2026-09-23

Onglet **Network** : disponibilité du RPC, du subsquid, du nœud POI et des broadcasters, sans wallet chargé.

### Ajouté
- `health.rs`, `POST /api/health/run` (tâche de fond dans le runtime HTTP) et `GET /api/health` :
  - RPC : `eth_chainId`, `eth_blockNumber`, latence, contrôle du chain id ;
  - subsquid : `squidStatus.height`, retard en blocs sur la tête RPC (warn > 20, down > 200), entité `transactions` interrogeable, présence des champs d'unshield, nombre total d'opérations ;
  - nœud POI : `ppoi_validated_txid` (index et merkleroot validés), retard en opérations = total subsquid − index validé (warn > 50), `ppoi_node_status_v2`, listes configurées présentes dans les listes servies ;
  - broadcasters : `railgun-broadcaster` sur nwaku REST (`subscribe`, `pump` pendant N s, `peer_count`, `all_quotes`) : broadcasters distincts, offres utilisables pour le jeton de base, messages reçus, pairs, détail des offres (frais, expiration, listes POI exigées, fiabilité) ; `skip` sans URL nwaku.
- Front : onglet Network avec URL nwaku et durée d'écoute, bouton Run checks, quatre cartes (niveau, résumé, latence, détail JSON), pastille de pire niveau dans l'onglet, ligne de journal par contrôle.
- Dépendance `railgun-broadcaster`.

### Notes
- Le transport « browser » (bridge js-waku du wallet) n'est pas repris : sans nwaku local, la sonde broadcasters est ignorée, les trois autres tournent.
- Les niveaux et seuils sont documentés dans DECISIONS.md (ADR-008).

## 0.1.7 — 2026-09-22

### Corrigé
- Blinded commitment d'unshield : la formule `poseidon(hash, npk, railgunTxid)` des 0.1.4 à 0.1.6 était fausse. Dans l'engine community, `BlindedCommitment.getForUnshield(railgunTxid)` renvoie le **railgun txid lui-même** (32 octets), et c'est sous cette clé, type `Unshield`, que le nœud enregistre le statut de l'opération. Le viewer sonde désormais le railgun txid directement pour toute opération avec unshield ou sans sortie déchiffrable ; plus besoin du destinataire, ni de l'event, pour le verdict. Les références chaîne (0.1.5, 0.1.6) ne servent plus qu'à l'affichage.

## 0.1.6 — 2026-09-22

### Corrigé
- Repli RPC pour les références d'opération : si le subsquid ne renvoie pas l'opération (ou la renvoie sans destinataire d'unshield), les logs du contrat Railgun au bloc de l'opération sont lus (`eth_getLogs`), la transaction est celle dont un log contient l'un de nos nullifiers, ses events `Unshield(to, token, amount, fee)` donnent la préimage (valeur brute = amount + fee), le timestamp vient de `eth_getBlockByNumber`. Aucune hypothèse sur le schéma subsquid ; deux appels RPC par opération, mis en cache.
- Cas confirmé sur les données du wallet receveur : deux unshields exacts sans change (opérations « unshield-only », `utxoTreeOut = 100000`), un seul commitment = hash de la préimage d'unshield, non déchiffrable par construction. Leur POI n'est vérifiable que sous le blinded commitment `poseidon(hash, npk = destinataire, railgunTxid)`, ce que la sonde d'unshield fait dès que le destinataire est connu.

## 0.1.5 — 2026-09-22

### Corrigé
- Transactions émises affichées sans date, sans hash et « no decryptable output » : les références chaîne dépendaient de requêtes subsquid par nullifier et par hash de commitment dont l'échec restait silencieux (terminal seulement). Les opérations sont maintenant lues par bloc dans l'entité `Transaction` du subsquid (la même que celle du txid indexer du SDK), appariées localement par nullifier : hash de transaction, timestamp et préimage d'unshield (`unshieldToAddress`, `unshieldToken`, `unshieldValue`) en sortent directement, avec repli sur un jeu de champs minimal si le schéma n'a pas les champs d'unshield. Les références des commitments et nullifiers de ces opérations en sont dérivées ; les requêtes par hash restent pour les shields et réceptions.
- Sonde POI de la sortie d'unshield : valeur de préimage prise dans `unshieldValue` (montant brut) plutôt que `amount + fee` de l'event.
- Toute erreur subsquid est désormais remontée dans le journal de l'interface.

## 0.1.4 — 2026-09-22

### Corrigé
- Transactions émises toutes marquées sans POI : le verdict ne reposait que sur les sorties déchiffrables (change, envois). Il intègre maintenant deux sources supplémentaires : le statut de la sortie d'unshield, sondé sous son propre blinded commitment `poseidon(poseidon(npk, tokenHash, montant + frais), npk, railgunTxid)` avec `npk` = adresse destinataire (type `Unshield`), et l'ensemble `recovered_valid` du SDK (opérations passées vues `Valid` par la passe de récupération, `RailgunProvider::poi_recovered_valid`).
- Une opération sans aucune sortie déchiffrable (unshield exact) n'est donc plus classée « sans POI » par construction.

### Ajouté
- Diagnostic : chaque transaction émise porte un objet `debug` (nullifiers et commitments de l'opération, entrées/sorties appariées avec leurs blinded commitments et statuts, statut d'unshield, indicateur recovered) affiché repliable dans le panneau de détail ; une ligne de journal par transaction émise au sync (`tx … inputs a/b outputs c/d unshield … pois … submitted … pending …`).
- Mnémonique saisie en clair (plus de masquage), le bouton Show / hide ne la concerne plus.
- `crypto` (Poseidon) en dépendance du viewer.

## 0.1.3 — 2026-09-22

### Modifié
- Front entièrement en anglais (formulaire, onglets, tableau, panneaux, graphe, anomalies, légende).
- Couleur des flèches du graphe selon le sens et la POI : entrante depuis une autre adresse en bleu, entrante avec POI valide ou soumise en violet, sortante en vert, sortante sans POI soumise en rouge ; shields et arêtes indéterminées en gris. Marqueurs de flèche assortis, export SVG inclus, `taint` rendu en pointillés sans changer la couleur.
- Historique : flèche colorée dans la colonne Type (↓ entrante, ↑ émise) avec les mêmes couleurs.

## 0.1.2 — 2026-09-22

### Corrigé
- Statuts POI tous « Inconnue » : `PoiProvider.pois` ne contient que les commitments pour lesquels le SDK a récupéré des preuves de Merkle (dépense), jamais les simples statuts. Le viewer interroge maintenant le nœud POI pour chaque note (reçues, dépensées, envoyées), liste par liste, via `PoiProvider::statuses_per_list` / `RailgunProvider::probe_poi` (SDK) ; les réponses `Valid` sont finales et conservées dans `viewer-cache.json` (clé `poi`), les autres sont resondées à chaque sync.

## 0.1.1 — 2026-09-22

### Corrigé
- `engine.rs` : conversion `UtxoLeafHash` → `U256` via `<U256 as From<_>>::from` (l'appel `U256::from` tombe sur la méthode inhérente `UintTryFrom` de ruint).
- `provider.rs` importait déjà `UtxoNote` en privé : l'import privé est retiré, le `pub use` le fournit au module et aux dépendants.
- Compilation : `railgun::note` est privé dans le SDK ; `UtxoNote`, `SentNote` et `blinded_commitment` sont ré-exportés depuis `railgun::provider` (comme `IndexedAccountState` et `Operation`) et importés de là par le viewer.
- `note_public_key` (SDK, désormais inutilisé) marqué `#[allow(dead_code)]`.

## 0.1.0 — 2026-09-22

Première version : crate séparée, exécutable `railgun-viewer` (daemon axum sur loopback + front embarqué).

### Ajouté
- Chargement depuis une mnémonique (dérivation railgun / kohaku, index), depuis une clé de visualisation privée + adresse 0zk, ou depuis la clé partageable de l'engine Railgun (msgpack `{vpriv, spub}`, point BabyJubJub dépaqueté à la circomlib).
- Signers view-only (`SpubSigner`, `MasterSigner`) satisfaisant `RailgunSigner` pour les chemins de lecture du SDK et refusant de signer.
- Resync via le SDK (`RailgunBuilder` + `WalletDb`, même disposition de données que `railgun-wallet`), POI en lecture seule pour les modes view-only.
- Historique : toutes les transactions du compte, émises et reçues, avec entrées, sorties, unshields, bilan par jeton, statut POI par liste, hash de tx et date via subsquid (cache disque), mémo par note et par transaction (panneau de détail).
- Onglet Arbre des notes : rendu NOXAKU réutilisé tel quel sur un snapshot `noxaku-notes-snapshot` construit côté Rust.
- Onglet POI manquantes : transactions émises sans `ProofSubmitted`/`Valid` sur aucune sortie, avec la raison, et file locale des preuves en attente.

### Modifié (crates/railgun, fork ZKNOX)
- `RailgunSigner` : méthodes par défaut `spending_pubkey()` et `master_public_key()`, `address()` passe par le master key ; `RailgunSignerError::new`.
- `UtxoNote::new` : npk calculé depuis `signer.master_public_key()` (`note_public_key_from_master`, public).
- `UtxoIndexer::account_state`, `RailgunProvider::{account_state, own_operations, poi_statuses}`, ré-exports `IndexedAccountState` et `Operation` (rendu public).
- `PoiProvider::{set_read_only, own_operations, statuses}` ; `RailgunBuilder::with_poi_read_only` (aucune génération, récupération ni soumission de preuve).
- Les appels `spending_key().public_key()` des chemins de lecture passent par `spending_pubkey()` ; ceux des circuits (`transact_inputs`) sont inchangés.

### Non validé
- Compilation à faire sur la machine cible (pas de toolchain Rust dans l'environnement de rédaction) ; les signatures alloy 1.x utilisées : `ProviderBuilder::new().network::<Ethereum>().connect().erased()`, `sol!` `#[sol(rpc)]` avec retours directs.
- Noms des champs subsquid (`blockTimestamp`, `transactionHash`, `unshields.token.tokenAddress`, `eventLogIndex`) : toute divergence laisse date, hash ou unshield vides, sans bloquer.

# railgun-viewer — décisions d'architecture

## ADR-013 — Waku Rust par défaut pour la sonde des broadcasters

Contexte : la sonde passait par le nœud js-waku de la page (ou un nwaku REST). Le wallet 0.13 a
un nœud Waku Rust natif (`waku-light` via `LightNodeTransport`), validé en réception et en light push.

Décision : le daemon du viewer démarre le même nœud (un par chaîne, gardé entre les vérifications)
et s'en sert par défaut ; js-waku reste en secours dans le sélecteur, et le bundle n'est chargé
qu'en mode onglet. `/api/defaults.nativeWaku` dit si le build a ce nœud : la version web répond
non pour l'instant et garde js-waku, jusqu'au passage de `waku-light` (wasm) dans son worker.

Amendement (viewer web 0.1.3) : la version web a désormais le même nœud, `waku-light` compilé en
wasm dans son Web Worker (WebSocket du navigateur), et répond `nativeWaku: true`.

## ADR-012 — Version web statique : mêmes sources, moteur wasm dans un Web Worker, front inchangé

Contexte : mettre Dr Rail en ligne sans serveur. Le SDK compile déjà en wasm (feature `js`,
`railgun-ts`), le front ne parle qu'à `/api/…`.

Décision : une crate `railgun-viewer-web` (cdylib wasm-bindgen) inclut par `#[path]` les modules
du viewer (`session`, `keys`, `chain`, `history`, `health`, `signer`, `shared`, `waku_link`) et le
fichier de dérivation du wallet ; seuls l'acteur et les routes sont réécrits pour le navigateur
(`web.rs`, même enchaînement de commandes que l'acteur natif). Le moteur tourne dans un Web Worker
(scan et déchiffrement sans figer la page) ; `shim.js` route les `fetch('/api/…')` du front vers lui.
Base du SDK et cache du viewer en IndexedDB, une base par chaîne et par compte, nommée comme le
répertoire du daemon. Le nœud js-waku de l'onglet Network reste dans la page, la liaison
`BrowserBridge` passe par le worker. Build et publication par GitHub Actions (Pages + artefact).

Conséquences : une modification du cœur vaut pour les deux cibles ; le code partagé ne doit pas
utiliser `std::time::{Instant, SystemTime}`, tokio ni le système de fichiers hors `cfg`. Le
navigateur exige le CORS des services appelés (RPC, subsquid, nœud POI) ; un service qui le refuse
dégrade la fonction correspondante (repli RPC pour le scan, statuts POI inconnus) sans bloquer.

## ADR-011 — Clé de visualisation : adresse 0zk facultative, découverte on-chain en repli (amende ADR-010)

Contexte : l'ADR-010 retirait l'adresse 0zk et comptait sur la lecture du master public key dans
la première note de transfert reçue en clair. Deux limites. Côté engine community, ce champ n'est
en clair que si l'expéditeur s'est masqué : il vaut `receiverMPK ^ senderMPK` quand l'expéditeur est
visible, et 0 sur les notes de change (`showSenderAddressToRecipient = true`), si bien qu'un compte
actif peut rester inouvrable. Le contrôle de hash prouve la cohérence de la note, pas l'appartenance
du MPK : une note forgée vers la clé de visualisation publique, avec un MPK arbitraire, le passe.

Décision : le mode « clé de visualisation » accepte une adresse 0zk facultative. Si elle est fournie,
le master public key en est lu, après contrôle que sa clé publique de visualisation est celle de la
clé privée saisie ; aucun parcours de la chaîne. Sans adresse, la découverte on-chain reste le repli.
L'adresse est publique, elle ne réduit pas la confidentialité de l'outil. Le champ mnémonique affiche
déjà la clé privée de visualisation et l'adresse dérivées, ce qui fournit les deux valeurs.

Reste ouvert : durcir la découverte (candidat retenu seulement s'il valide une note dont le
nullifier figure sur la chaîne ; chemin receveur uniquement).

Amendement 0.2.15 : l'adresse est une source de clé à part entière dans le sélecteur (« Private viewing key + 0zk address », adresse obligatoire), distincte de « Private viewing key only » qui garde la découverte on-chain et n'envoie jamais d'adresse.

## ADR-001 — Crate séparée sur le même SDK et le même format de données que le wallet

Le viewer est un binaire distinct de `railgun-wallet` mais dépend de lui comme bibliothèque
(`default-features = false` : pas d'axum du wallet) pour la dérivation des clés et la base sur disque.
Il partage la disposition `<chain>/<sha256(adresse)[..8]>/db-v2`, ce qui permet de pointer
`--data-dir ~/.railgun-wallet` et d'éviter un second scan. Le prix : ne pas faire tourner les deux
sur le même répertoire simultanément (stockage fichier-par-clé, sans verrou).

## ADR-002 — View-only par le master key de l'adresse, pas par la clé de dépense publique

`npk = poseidon(mpk, random)` et `mpk = poseidon(spub.x, spub.y, nk)`. Une clé de visualisation
seule ne permet ni de recalculer les npk, ni les hashes, ni les blinded commitments (donc pas de
statut POI). Plutôt que d'exiger la clé de dépense publique, le viewer prend l'adresse 0zk, qui
contient le master key. Cela demande au SDK une méthode `master_public_key()` sur `RailgunSigner`,
utilisée par `UtxoNote::new` et `address()`. La clé partageable de l'engine Railgun, qui embarque
`spub`, reste acceptée (dépaquetage circomlib du point BabyJubJub).

## ADR-003 — POI en lecture seule pour les signers view-only

Le fork récupère et soumet les preuves POI manquantes au sync (`recover_missing`,
`submit_pending`). Un signer view-only produirait des entrées de preuve invalides (clé de dépense
publique factice). `RailgunBuilder::with_poi_read_only` garde le txid-tree et les statuts, coupe
génération, récupération et soumission. En mode mnémonique le viewer conserve le comportement du
wallet : il répare les POI manquantes.

## ADR-004 — Références chaîne par subsquid, pas par `eth_getLogs`

Le SDK ne conserve ni hash de transaction, ni timestamp, ni détail d'unshield. Les récupérer par
logs RPC demanderait de connaître les blocs des shields (inconnus) ou de balayer l'historique. Le
subsquid Railgun répond par hash de commitment et par nullifier en une requête, et sert déjà au
sync. Les réponses sont mises en cache (`viewer-cache.json`) ; toute défaillance laisse les champs
vides sans bloquer l'historique.

## ADR-010 — Clé de visualisation seule : master key retrouvé sur la chaîne (remplace ADR-002)

L'ADR-002 demandait l'adresse 0zk en plus de la viewing key. Retirée à la demande de l'utilisateur.
Le master public key est désormais lu dans les notes elles-mêmes : le premier champ du bundle
chiffré d'une note de transfert porte le master key du receveur, en clair chez kohaku, et chez
l'engine Railgun tant que l'expéditeur ne se révèle pas (auquel cas il est XORé avec le sien).
Le candidat n'est retenu que s'il reproduit le hash on-chain, ce qui rend la lecture sûre. Coût :
un parcours complet des commitments au premier chargement, avec les mêmes syncers que le sync
normal. Limite assumée : un compte qui n'a jamais reçu de transfert en clair ne peut pas être
ouvert par sa seule viewing key ; l'interface le dit et renvoie à la mnémonique.

## ADR-009 — Broadcasters via le nœud Waku de l'onglet, comme le wallet

Il n'existe pas de client Waku léger natif en Rust ; le wallet fait tourner js-waku dans son onglet
et le relie au daemon par `BrowserBridge`. Le viewer reprend exactement ce montage (même bundle,
mêmes routes `/api/defaults` et `/api/waku/exchange`) plutôt que d'exiger un nœud nwaku local :
la sonde broadcasters observe ainsi le même réseau que le wallet, avec le même transport. Le
nwaku REST reste disponible en renseignant l'URL. Le jour où un transport Rust remplace js-waku
dans `railgun-broadcaster`, le viewer en hérite sans changement.

## ADR-008 — Sondes réseau : mesures brutes, seuils explicites, aucune dépendance au wallet

Les quatre sondes tournent dans le runtime HTTP, sans clé ni moteur, pour rester utilisables
avant tout chargement. Elles mesurent ce dont le viewer et le wallet dépendent réellement :
la tête RPC (référence), le retard du subsquid en blocs (source du txid-tree et des références
chaîne), le retard du nœud POI en opérations (index validé contre le total d'opérations indexées ;
tant qu'une opération n'est pas validée, sa preuve est irrecevable, ce qui explique les
`ProofSubmitted` qui traînent), et la présence de broadcasters vivants pour le jeton de base.
Seuils : subsquid warn > 20 blocs, down > 200 ; POI warn > 50 opérations, down si une liste
configurée n'est pas servie ; broadcasters warn si aucun signataire ou aucune offre utilisable.
Le transport broadcasters est nwaku REST, celui du wallet en mode « nwaku » ; le bridge
navigateur est hors périmètre.

## ADR-007 — Statut POI d'un unshield : clé = railgun txid (engine community)

Vérifié dans l'engine community (`blinded-commitment.ts`) : pour un unshield, le blinded
commitment soumis et interrogé est `formatToByteLength(railgunTxid, 32)`, alors que pour une note
c'est `poseidon(hash, npk, position globale)`. Le viewer aligne sa sonde sur ce comportement
(type `Unshield`, valeur = railgun txid), ce qui rend le verdict des unshields totaux indépendant
de toute reconstruction de préimage et de toute source chaîne. L'ADR-006 reste valable pour les
deux autres sources.

## ADR-006 — Verdict POI d'une transaction émise : trois sources

Le nœud POI ne connaît une opération que par ses sorties : blinded commitments des notes créées
(`poseidon(hash, npk, position globale)`) et, pour un unshield, un blinded commitment propre
`poseidon(hash, npk, railgunTxid)` avec `npk` = adresse destinataire. Une opération sans sortie
déchiffrable (unshield exact) n'a donc aucune note à sonder. Le viewer combine : les statuts des
sorties déchiffrées (change, envois), le statut de la sortie d'unshield reconstruite depuis
l'event `Unshield` (montant + frais), et l'ensemble `recovered_valid` du SDK. Le meilleur statut
par liste l'emporte ; `ProofSubmitted` ou `Valid` sur l'une de ces sources suffit à sortir la
transaction de l'onglet Missing POI. Les entrées de ce verdict sont exposées (`debug`) pour que
tout désaccord avec le wallet soit lisible sans instrumentation.

## ADR-005 — Front : rendu de lignée réutilisé, modèle calculé en Rust

Le graphe reprend le script NOXAKU tel quel (contrat `noxaku-notes-snapshot` v1) ; le snapshot est
produit par `history::graph_snapshot`. Tout le modèle métier (transactions, sens, bilans, POI par
liste, détection des émissions sans preuve) est en Rust, testable sans moteur ; le front ne fait
qu'afficher.

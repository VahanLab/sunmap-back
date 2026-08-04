# AGENTS.md — Projet SunShadow

## Suivi de projet (Notion)

Le suivi des tâches vit dans la base Notion **« Développements »** (workspace
Sunmap). **Toute lecture ou écriture Notion passe par le serveur MCP
`notion-sunmap`** (`.mcp.json`, jeton propre à ce workspace) — jamais par un
autre connecteur Notion éventuellement configuré par ailleurs, qui pointerait
sur un workspace différent.

Tâches bloquantes pour la mise en production : préfixe **`[MEP]`** dans le
titre. Faute d'endpoint de modification de schéma fonctionnel sur ce serveur
MCP (`API-update-a-data-source` répond `invalid_request_url` quelle que soit
la version d'API testée), le tag n'est pas une propriété structurée mais ce
préfixe — à corriger si l'endpoint se débloque un jour.

## Vision produit

App mobile pour visualiser sur une carte la position du soleil et les ombres
projetées **dynamiquement** (pas de shadows précalculées), afin de trouver dans
une ville les endroits au soleil ou à l'ombre. Inspirée du site web
"Where is the sun" / ShadeMap.

Fonctionnalités cibles :
- Carte avec ombres projetées tenant compte du **relief, des bâtiments ET de
  la végétation** (canopée semi-transparente, en place côté calcul).
- **Timeline/slider** : faire défiler les heures et les jours de l'année et
  voir les ombres évoluer de façon fluide.
- **Objectif final produit** : brancher une API bars/restaurants avec terrasse
  (tag OSM `outdoor_seating=yes`, enrichissable Google Places) et filtrer
  ceux au soleil vs à l'ombre à un instant donné — avec des infos du type
  "au soleil jusqu'à 18h40".

## Qui calcule quoi — répartition des responsabilités (état actuel)

**Serveur (`helios-server`, Rust/axum) — la vérité soleil/ombre.**
- Assemble la DSM par bbox : tuiles DEM Mapterhorn + rasterisation des
  bâtiments PostGIS (grille opaque) + végétation en couche canopée séparée
  (`canopy_top`/`canopy_base`), et la grille `owner` (qui occupe chaque
  cellule — sert au blocker nommé et au recalage hors bâtiment).
- Ray marching (`helios-core`) : classification soleil/ombre de chaque lieu,
  bâtiments opaques, canopée traversée par transmittance (0,6/m, seuil 25 %).
- **Bitfield `sun_day`** dans `GET /places` : la journée locale entière de
  chaque lieu (144 tranches de 10 min, 1 bit = soleil), calculée sur la même
  DSM en un passage. C'est lui qui rend le slider client autonome.
- Recalage des nœuds d'établissement hors bâtiment (`nudge_out_of_building`),
  positions de terrasse contribuées prises telles quelles, mobilier jamais
  recalé.
- Frise d'un point (`/sun-hours`), debug (`/debug/ray`), contributions
  (terrasse, mobilier, historiques), traductions (i18n).

**Client (app iOS SunMap) — le rendu, et la relecture locale.**
- Ombres **visuelles** : lumières Mapbox (`DirectionalLight` pilotée par
  `SunPosition.swift`, port 1:1 de `sun.rs`) pour les bâtiments et le
  mobilier 3D, plus le masque d'ombre **terrain** calculé en Metal
  (`ShadowEngine` + `Shaders.metal`, port du ray marching, terrain seul —
  ni bâtiments ni végétation dans la DSM client).
- Classification des lieux : **relue localement** depuis `sun_day`
  (`Place.reclassified(at:)`) à chaque cran du slider — zéro requête tant
  qu'on reste sur la même journée et la même zone ; une requête `/places`
  par (zone, jour). La nuit se déduit de l'élévation solaire locale.
- Végétation visuelle **entièrement maison** (les arbres 3D du style Mapbox
  sont éteints) : arbres isolés et bosquets d'emprises boisées, silhouette
  choisie par `leaf_type` (feuillu / conifère / palmier), implantés dès z15
  depuis les tuiles de canopée (`/canopy/{z}/{x}/{y}`, canal B = classe) ;
  sous z15, le masque Metal ombre toute la canopée par transmittance.
  Détail : `ios/SunMap/AGENTS.md` § « Végétation, partage du rendu ».

**Écarts rendu/calcul assumés (et leurs limites connues).**
- Les ombres visibles (Mapbox) et la classification (notre DSM) ne viennent
  pas des mêmes données pour les bâtiments : désaccords possibles en bord
  d'ombre.
- **Végétation : écart résorbé.** Arbres isolés comme emprises boisées sont
  désormais rendus ET calculés depuis nos données (tables `trees`/`woods`,
  servies en tuiles de canopée) — ce qu'on voit est ce qui fait l'ombre.

## Décisions d'architecture (historique et état actuel)

1. React Native envisagé puis **abandonné** → développement **natif Swift iOS**
   (Kotlin Android ensuite).
2. Base map : **Mapbox SDK v11** (choisi contre MapLibre Native car MapLibre
   n'a pas encore le terrain 3D sur mobile — PR "3d terrain work" encore en
   draft en juillet 2026).
3. Moteur d'ombres : **crate Rust `helios-core`** (zéro dépendance) —
   position solaire NOAA + DSM (heightmap + couche canopée) + ray marching
   à transmittance. Sert à la fois le rendu (masque raster) et la query
   ("ce point est-il à l'ombre à t ?").
4. Stratégie de rendu **tranchée : piste A retenue** — API lumière du SDK
   Mapbox v11 (`DirectionalLight`, `castShadows`) pilotée par la position
   solaire, complétée d'un masque Metal pour l'ombre portée du relief.
   Limites assumées : données de rendu (Mapbox) ≠ données de query (DSM
   Rust) → désaccords possibles en bord d'ombre. Le serveur sert les queries
   (classification, frises, cumuls), pas de tuiles d'ombre.
   La piste B (custom layer Metal complet, relief + bâtiments) reste le
   recours si les désaccords deviennent gênants.
5. Serveur : Rust/axum, **en place** (cf. `helios-server/README.md`) :
   - `GET /sunlit` + `POST /sunlit/batch`, `GET /places` (avec `sun_day`),
     `POST/PUT /places/furniture` + `GET /places/furniture/contributions`,
     `POST /places/terrace` + `GET /places/terrace/contributions`,
     `GET /sun-hours`, `GET /trees`, `GET /debug/ray`, comptes/profils.
6. Géométrie OSM d'abord en **PostgreSQL/PostGIS** (plus d'Overpass au
   runtime — 5-20 s par bbox dense, 504 fréquents), puis **sortie de la base
   au profit de l'archive vectorielle** `sunmap.pmtiles` (2026-08) : les
   tables `buildings`/`trees`/`woods` sont supprimées, la géométrie va de
   l'extrait OSM à l'archive (`bin/tilegen`) et le serveur la lit là
   (`vtiles.rs`, `VECTOR_TILES` obligatoire). PostgreSQL ne garde que le
   métier : lieux, comptes, contributions. Schéma : `helios-server/schema.sql`.
7. **Alimentation par extrait PBF + osmium** (`scripts/osm-extract.sh`),
   jamais par Overpass. Overpass par tuiles demandait 192 requêtes et
   ~45 min pour Paris seul, avec 27 échecs au premier essai — irréaliste à
   l'échelle de la France, et abusif envers un service gratuit partagé.
   L'extrait Geofabrik se traite en local en quelques minutes ; rafraîchir
   une zone = reprendre l'extrait du jour (quotidien chez Geofabrik —
   `bin/ingest` a disparu avec les tables). Les règles tags → hauteur
   (`osm::building_from`, `osm::height_from_tags`) vivent dans `osm.rs`/
   `pbf.rs` : ne jamais les dupliquer, elles ont coûté cher à mettre au
   point.
8. Animation fluide du slider — **tranchée par le bitfield `sun_day`**
   (variante par-lieu de l'option « bitfield temporel » envisagée) : le
   serveur calcule la journée entière de chaque lieu en un passage de DSM
   (~6-9 ms pour une centaine de lieux × 144 tranches), le client reclasse
   localement à chaque cran. Une requête par (zone, jour) au lieu d'une par
   tranche de 5 min ; 36 octets hex par lieu. Les tuiles raster d'ombre
   (options cross-fade / shader client) n'ont plus de raison d'être pour la
   classification — elles ne restent pertinentes que si un masque d'ombre
   *visuel* complet devenait nécessaire (piste B).

## Sources de données (toutes open data)

> État de l'art des données d'élévation, alternatives évaluées et résultats de
> mesure : **`docs/recherche-donnees-elevation.md`**. À lire avant d'envisager
> de changer de source. Conclusion courte : aucun DSM public mondial n'est
> assez fin (le meilleur gratuit est à 30 m), notre assemblage DEM + emprises
> est l'état de l'art, mais **Mapterhorn s'arrête à z12 hors pays à LiDAR
> ouvert** — le serveur y tombe en 502 aujourd'hui.

- **Terrain (monde)** : tuiles DEM **Mapterhorn**
  (`https://tiles.mapterhorn.com/{z}/{x}/{y}.webp`, webp 512 px, encodage
  Terrarium `alt = r*256 + g + b/256 − 32768`, maxzoom 16 vérifié France,
  z17 partiel, attribution « © Mapterhorn »). Remplace l'ancien duo
  AWS `elevation-tiles-prod` (compute) + `mapbox-terrain-dem-v1` (rendu iOS) :
  une seule source pour rendu ET query.
- **Bâtiments (en place)** : OSM via Overpass à l'ingestion — `way[building]`,
  `way[building:part]` (Simple 3D Buildings) ET `relation[building]` (les
  multipolygones à cour, majoritaires à Paris ; les oublier retirait 41 % des
  casters). Hauteur : `height`, sinon `building:levels × 3 m + 3 m` de
  toiture, sinon **médiane locale** du quartier (~30 % des bâtiments
  parisiens n'ont aucun tag ; un défaut global de 9 m les sous-estimait de
  moitié). Alternatives non retenues pour l'instant : Overture Buildings
  (GeoParquet, hauteurs ML), tuiles OpenMapTiles (`render_height`).
- **France (arme absolue)** : IGN LiDAR HD open data — MNS/MNT/MNH, dalles
  GeoTIFF 1 km × 1 km au pas de 50 cm. Le MNS = terrain + bâtiments +
  végétation → DSM directe sans fusion. Couverture ~80 % fin 2025, complète
  prévue fin 2026.
- **Végétation (étape 2 en place — transmittance)** : arbres isolés
  (`natural=tree`, disque de rayon de couronne) et emprises boisées
  (`natural=wood`, `landuse=forest`, `tree_row`, `scrub`) tamponnés dans une
  **couche canopée dédiée** de la DSM (`canopy_top`/`canopy_base`), séparée
  des obstacles opaques. Le ray marching **traverse** la couronne en
  atténuant (`canopy_transmittance_per_m`, défaut 0,6/m ; seuil de lumière
  25 %) au lieu de s'arrêter : un arbre d'alignement laisse passer le soleil
  sur ses bords, une futaie l'éteint. Le rayon passe librement **sous la base
  du houppier** (base = sommet − diamètre de couronne pour un arbre isolé, le
  sol pour un bois). Motivé par un cas réel : une terrasse à 1,3 m d'un
  platane passait de « soleil l'après-midi » à « 0 h par jour » avec le
  tamponnage opaque (`node/653366336`). Hauteurs : tag `height` s'il existe,
  sinon repli par type (futaie 18 m, alignement 12 m, broussailles 3 m) —
  63 % des arbres portent la valeur par défaut de 10 m. **Silhouette** :
  tag `leaf_type` (`broadleaved`/`needleleaved`/`palm`), à défaut déduite du
  `genus`/`species` — les imports municipaux français renseignent souvent le
  genre sans le type de feuillage. Sert à choisir le modèle 3D côté client.
  Étapes suivantes : hauteur réelle (Meta/WRI CHM ou IGN MNH), τ saisonnier
  par `leaf_type` (feuillu d'hiver quasi transparent). Note : `Shaders.metal`
  (port client de `shadow.rs`) n'a pas la logique canopée — la DSM client est
  terrain seul, sans donnée de végétation à traverser.
- **Arbres (backlog)** : Meta/WRI Canopy Height Map v2 (2026, ~1 m, COG
  EPSG:3857 sur AWS) ; OSM `natural=tree` (riche en France via imports
  municipaux), `tree_row`, `wood`/`forest`.
- **Mobilier urbain (en place)** : bancs (`amenity=bench`) et tables de
  pique-nique (`leisure=picnic_table`), dans la **même table `places`** et le
  même endpoint que les établissements — la question « au soleil à quelle
  heure ? » y est identique. Deux différences assumées : **aucun recalage hors
  bâtiment** (un banc est cartographié là où il est, contrairement au nœud d'un
  bar posé sur son immeuble), et cinq attributs propres — `direction`
  (orientation du regard assis, croisable avec l'azimut solaire), `covered`,
  `backrest`, `seats`, `material`. Volume : ~49 500 bancs et ~2 900 tables pour
  l'Île-de-France, d'où un affichage **éteint par défaut** côté client.
  Les aires de jeux (`leisure=playground`) restent au backlog : polygones, donc
  centroïde ou échantillonnage d'emprise à trancher.
- **Établissements (en place)** : OSM `amenity` = bar, pub, restaurant, cafe,
  fast_food, biergarten (`osm::AMENITIES`). **Pas de filtre sur
  `outdoor_seating`** à l'ingestion : le tag manque sur ~79 % des
  établissements parisiens, filtrer dessus en écartait la majorité. Il est
  stocké tel quel et le filtre est laissé au client.

## État du code

### `helios-core/` — crate Rust (zéro dépendance, testé unitairement)

- `src/sun.rs` : position solaire NOAA (~0,01°). Référence de test :
  Paris 2026-06-21 12:00 UTC → élévation ≈ 64,6°, azimut ≈ 180°.
- `src/dsm.rs` : struct `Dsm` (grille f32, row 0 = nord, y vers le sud),
  `from_terrarium_rgb`, échantillonnage bilinéaire, `stamp_max` (rasterisation
  rectangulaire de bâtiments — la vraie rasterisation polygone scanline est à
  faire dans le pipeline data).
- `src/shadow.rs` : `is_shadowed` (ray marching, early-exit sur altitude max,
  `observer_height_m` paramétrable — cas "tête au soleil, pieds à l'ombre"
  pour les terrasses) et `render_mask` (255 = ombre). Parallélisation rayon
  à ajouter côté serveur (`par_chunks_mut` par lignes).
- `Cargo.toml` : dépendances serveur commentées (rayon, image, tokio, axum).
- **Les tests n'ont pas encore été exécutés** (écrits mais non lancés) :
  première chose à faire → `cargo test`.

### `ios/SunShadowPOC/` — POC SwiftUI Mapbox v11 (piste A)

- `SunPosition.swift` : **port 1:1 de `sun.rs`** — garder les deux synchro
  (mêmes valeurs de référence).
- `ShadowMapView.swift` : MapView v11, terrain 3D (`mapbox-terrain-dem-v1`),
  bâtiments extrudés (source composite, filtre `extrude=true`), `setLights`
  à chaque tick du slider (polaire = 90 − élévation, borné à 88° pour éviter
  les artefacts au soleil rasant), teinte réchauffée au couchant.
- `ContentView.swift` : slider 00:00–23:55 (pas 5 min) + DatePicker.
- `SunShadowPOCApp.swift` : entry point.
- Setup : projet Xcode vierge + SPM `mapbox-maps-ios` v11 + token
  `MBXAccessToken` dans Info.plist. Tester sur device (terrain lourd en simu).
- **À évaluer sur device** : fluidité du slider, artefacts au soleil rasant,
  rendu global. Ce test décide entre piste A et piste B.

## Fait (jalons passés, garder pour l'historique)

- helios-core testé (14 tests), serveur axum complet, base PostGIS remplie
  par extrait PBF (Île-de-France).
- Piste A validée sur device et retenue ; masque Metal terrain en place.
- Contributions : terrasses (présence + position), mobilier urbain (ajout +
  correction), historiques par lieu, comptes/pseudos/paliers.
- Bitfield `sun_day` (slider sans réseau) et transmittance de canopée.

## Remontée vers OpenStreetMap

Un compte peut être **lié à OSM** pour que les contributions corrigent la carte
source, et pas seulement notre base : `outdoor_seating` sur l'établissement
existant, nœuds `amenity=bench` et `leisure=picnic_table` pour le mobilier.

Le jeton d'écriture vit **côté serveur** : c'est lui qui pousse, en différé si
OSM est indisponible, et un jeton baladé dans l'app ne serait plus révocable.
Les envois passent par une file (`osm_pushes`) et jamais par le chemin de la
requête — la carte SunMap doit rester juste même quand OSM ne répond pas.

Un **changeset par contribution**, avec commentaire et `created_by=SunMap` :
c'est ce que la communauté attend d'un éditeur tiers. Une modification
d'élément existant relit sa version courante avant d'écrire, et n'ajoute que le
tag concerné — l'API refuse une écriture périmée, ce qui protège du travail
d'autrui.

**Essayer sur `https://api.dev.openstreetmap.org` d'abord** (`OSM_API_BASE`) :
une erreur en production salit une base que des milliers de gens relisent à la
main. Protocole complet : `helios-server/README.md`.

## Importer une nouvelle zone (procédure réutilisable)

Ajouter une zone = **une commande**, qui enchaîne téléchargement PBF →
extraction osmium → `bin/tilegen` (extrait → `tiles/sunmap.pmtiles`,
l'archive vectorielle unique, MVT z14, couches `buildings`/`woods`/`trees`,
**sans base de données**) → `bin/import` (lieux seuls vers PostgreSQL) :

    scripts/import-zone.sh <URL Geofabrik | zone.osm.pbf> [--upload]

Procédure détaillée, vérifications et configuration R2 :
**`docs/import-zone.md`**. Format et service de l'archive :
`docs/tuiles-pmtiles.md`. À savoir : **les tables géométriques n'existent
plus** (migration `drop_geometry_tables`) — PostgreSQL ne porte que lieux,
comptes et contributions, et `VECTOR_TILES=tiles/sunmap.pmtiles` est
obligatoire au démarrage du serveur. L'archive ne couvre que l'extrait
donné (plus de base cumulative) : pour plusieurs zones, prendre un extrait
englobant (ex. `france-latest`). L'import des lieux reste un upsert.
`--upload` pousse sur Cloudflare R2 (`scripts/r2-upload.py`, variables
`R2_*` dans `helios-server/.env`). Parité mesurée à la bascule : `/sunlit`
identique, `/canopy` à ~0,1 % de pixels d'écart (quantification ~0,6 m),
arbres à ±0,2 m.

## Prochaines étapes (dans l'ordre)

1. Échantillonner la terrasse en surface plutôt qu'au point : 3–5 points dans
   un buffer de 3–8 m, renvoyer un % d'ensoleillement plutôt qu'un booléen.
2. **Arbres affichés maison** (différé, cf. « Écarts rendu/calcul ») :
   demande d'abord un pipeline de **vector tiles** pour nos arbres — les
   requêtes bbox au déplacement de carte sont exclues. À concevoir : tuilage
   (tippecanoe ou serveur MVT depuis PostGIS), puis `ModelLayer` sur cette
   source, en superposition ou en remplacement des arbres Mapbox.
3. Cache CDN des réponses `/places` (clé zone + jour, le bitfield rend la
   clé stable une journée entière).
4. Tuiles statiques sur Cloudflare R2 : `sunmap.pmtiles` (vectoriel, cf.
   § « Importer une nouvelle zone ») alimente déjà le serveur via
   `VECTOR_TILES` ; reste à brancher le client dessus — masque Metal
   (rasterisation GPU de la même géométrie) et arbres 3D (`ModelLayer`),
   ce qui couvre aussi l'étape 2 ci-dessus.

## Backlog (décidé mais volontairement différé)

- **Végétation, suite** : hauteurs réelles de canopée (Meta/WRI CHM v2,
  IGN MNH), τ saisonnier par `leaf_type` (feuillu d'hiver quasi transparent).
- Android (Kotlin) après validation iOS.
- Cumul annuel d'ensoleillement par point ("cette terrasse est au soleil
  220 jours par an à 12h").

## Conventions

- `sun.rs` et `SunPosition.swift` doivent rester des ports exacts l'un de
  l'autre ; toute modif de l'un se répercute sur l'autre + tests.
- Grille DSM : x vers l'est, y vers le sud (ligne 0 = bord nord).
- Azimut : degrés depuis le nord, sens horaire. Élévation : degrés au-dessus
  de l'horizon.
- helios-core reste **zéro dépendance** (portable serveur / mobile via
  UniFFI / WASM) ; les dépendances vivent dans les binaires (serveur, CLI).
- Langue du projet : commentaires et docs en français.

## Sous-projets

- **App iOS SunMap** : voir `ios/SunMap/AGENTS.md` pour l'architecture SwiftUI,
  les features, l'auth Firebase et les spécificités Xcode.
- **Fiche App Store** : `tools/asc/` — outil Node sans dépendance qui pousse
  textes et captures dans les 5 langues via l'API App Store Connect. Les textes
  sont versionnés en `tools/asc/metadata/<locale>/*.txt`, les captures en
  `tools/asc/screenshots/<locale>/<type d'écran>/`. Mode d'emploi :
  `tools/asc/README.md`.

# AGENTS.md — Projet SunShadow

## Vision produit

App mobile pour visualiser sur une carte la position du soleil et les ombres
projetées **dynamiquement** (pas de shadows précalculées), afin de trouver dans
une ville les endroits au soleil ou à l'ombre. Inspirée du site web
"Where is the sun" / ShadeMap.

Fonctionnalités cibles :
- Carte avec ombres projetées tenant compte du **relief ET des bâtiments**
  (les arbres viendront plus tard, voir "Backlog").
- **Timeline/slider** : faire défiler les heures et les jours de l'année et
  voir les ombres évoluer de façon fluide.
- **Objectif final produit** : brancher une API bars/restaurants avec terrasse
  (tag OSM `outdoor_seating=yes`, enrichissable Google Places) et filtrer
  ceux au soleil vs à l'ombre à un instant donné — avec des infos du type
  "au soleil jusqu'à 18h40".

## Décisions d'architecture (historique et état actuel)

1. React Native envisagé puis **abandonné** → développement **natif Swift iOS**
   (Kotlin Android ensuite).
2. Base map : **Mapbox SDK v11** (choisi contre MapLibre Native car MapLibre
   n'a pas encore le terrain 3D sur mobile — PR "3d terrain work" encore en
   draft en juillet 2026).
3. Moteur d'ombres : **crate Rust `helios-core`** (zéro dépendance) —
   position solaire NOAA + DSM (heightmap) + ray marching. Sert à la fois le
   rendu (masque raster) et la query ("ce point est-il à l'ombre à t ?").
4. Stratégie de rendu en cours de validation (POC iOS) :
   - **Piste A (en test)** : API lumière du SDK Mapbox v11
     (`DirectionalLight` + `AmbientLight`, `castShadows`) pilotée par la
     position solaire. Ombres de bâtiments réelles et fluides au slider.
     Limites connues : le relief ne projette PAS d'ombre portée (seulement
     hillshade aligné sur la lumière) ; aucune query possible ; données de
     rendu (Mapbox) ≠ données de query (DSM Rust) → désaccords possibles en
     bord d'ombre.
   - **Piste B (fallback/cible si A insuffisante)** : custom layer **Metal**
     rendant notre propre masque d'ombre (même algo que helios-core) —
     relief inclus, cohérence parfaite rendu/query.
   - Si la piste A suffit visuellement, le serveur Rust ne sert **que** les
     queries terrasses (batch + cumuls).
5. Serveur : Rust/axum, **en place** (cf. `helios-server/README.md`) :
   - `GET /sunlit` + `POST /sunlit/batch`, `GET /places`,
     `POST /places/terrace` (contribution utilisateur), `GET /sun-hours`,
     `GET /trees`, `GET /debug/ray`.
   - Reste à faire : `GET /shadow/{z}/{x}/{y}.png?t=` (tuile raster, si piste
     B/web) et le cache CDN clé `(z,x,y,jour,tranche de 5-10 min)`.
6. **Géométrie OSM en PostgreSQL/PostGIS**, plus d'Overpass au runtime.
   Overpass met 5-20 s par bbox dense, répond 504 aux heures de pointe et
   impose une politesse incompatible avec une requête par déplacement de
   carte. Requêtes par bbox servies par index GIST. Schéma :
   `helios-server/schema.sql`.
7. **Remplissage de la base par extrait PBF + osmium** (`scripts/osm-extract.sh`
   puis `bin/import`), pas par Overpass. Overpass par tuiles demandait 192
   requêtes et ~45 min pour Paris seul, avec 27 échecs au premier essai —
   irréaliste à l'échelle de la France, et abusif envers un service gratuit
   partagé. L'extrait Geofabrik se traite en local en ~1 min. `bin/ingest`
   (Overpass) est conservé pour rafraîchir une petite zone. Les deux écrivent
   les mêmes identifiants et partagent les mêmes règles tags → hauteur
   (`osm::building_from`, `osm::height_from_tags`) : ne jamais dupliquer ces
   règles, elles ont coûté cher à mettre au point.
6. Animation fluide du slider (préoccupation UX forte) — options par ordre :
   1. Cross-fade de rasters préfetchés (pas de 10 min) — MVP, drapé auto sur
      terrain Mapbox.
   2. Tuiles "bitfield temporel" (1 bit soleil/ombre par tranche packé en
      RGBA) + petit shader client — 1 requête par jour, scrub 60 fps.
   3. Ray marching complet dans un shader client (tuiles DSM servies par le
      serveur) — fluidité continue, mais custom layer + terrain Mapbox = le
      point délicat.

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
- **Végétation (étape 1 en place)** : arbres isolés (`natural=tree`, disque de
  rayon de couronne) et emprises boisées (`natural=wood`, `landuse=forest`,
  `tree_row`, `scrub`) tamponnés dans la DSM, donc porteurs d'ombre. Hauteurs :
  tag `height` s'il existe, sinon repli par type (futaie 18 m, alignement 12 m,
  broussailles 3 m) — OSM ne tague quasiment jamais la hauteur d'un bois, et
  63 % des arbres portent la valeur par défaut de 10 m.
  **Limite assumée** : la canopée est tamponnée comme un volume plein, donc
  opaque. Juste pour une futaie dense en été, faux pour des feuillus en hiver.
  Les étapes suivantes sont la hauteur réelle (Meta/WRI CHM ou IGN MNH) puis la
  transmittance, qui demande un ray marching cumulatif et une classe portée par
  la DSM.
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

## Prochaines étapes (dans l'ordre)

1. `cargo test` sur helios-core, corriger si besoin.
2. Monter le POC iOS dans Xcode, tester sur device, trancher A vs B.
3. Serveur axum : endpoint `/sunlit` + batch (query-only d'abord), chargement
   des tuiles Terrarium avec marge (les casters hors tuile — marge fonction
   de l'élévation solaire : soleil bas = ombres longues = marge large).
4. Pipeline data : décodage PNG Terrarium (crate `image`), rasterisation
   polygone des emprises Overture/OSM, cache DSM (PMTiles ou S3).
5. **Position de terrasse contribuée par les utilisateurs** (`place_terraces`,
   table séparée pour survivre aux réimports OSM) : c'est la seule donnée qui
   situe vraiment une terrasse, OSM ne donnant que le nœud du bâtiment.
   Échantillonner
   3–5 points dans un buffer de 3–8 m côté rue plutôt que le centroïde du
   bâtiment ; renvoyer un % d'ensoleillement plutôt qu'un booléen.
6. Si piste B retenue : custom layer Metal (drapage sur terrain = le sujet
   difficile, à prototyper tôt).

## Backlog (décidé mais volontairement différé)

- **Arbres** : couche de classes dans la DSM (sol/bâtiment/canopée), ombre de
  canopée semi-transparente, atténuation saisonnière (feuillus en hiver,
  tag OSM `leaf_type`). Sources : Meta/WRI CHM v2 + IGN MNS + OSM.
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

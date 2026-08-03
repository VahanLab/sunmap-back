# `sunmap.pmtiles` — l'artefact vectoriel unique, servi depuis Cloudflare R2

Une seule archive PMTiles, tuiles **MVT** (Mapbox Vector Tiles), qui porte
toute la géométrie qui fait de l'ombre. Trois consommateurs, mêmes octets :

1. **le serveur** (`helios-server/src/vtiles.rs`, `VECTOR_TILES=`) — la
   rasterisation DSM pour la classification soleil/ombre ;
2. **le masque Metal client** (à venir) — rasterisation GPU de la même
   géométrie ;
3. **l'affichage Mapbox** (à venir) — arbres 3D (`ModelLayer`), extrusions.

Ce qu'on voit est ce qui fait l'ombre, par construction. Les tables PostGIS
`buildings`/`trees`/`woods` **n'existent plus** (migration
`drop_geometry_tables`) : l'archive est générée par `bin/tilegen` directement
depuis l'extrait OSM, PostgreSQL ne garde que le métier — lieux, comptes,
contributions (cf. `docs/import-zone.md`).

## Format

- **Un seul zoom : z14**, extent 4096 → pas de quantification ~0,6 m, petit
  devant le pixel DSM (~1,57 m). Les lecteurs sur/sous-échantillonnent
  librement — le vectoriel n'a pas de résolution. Vérifié en parité contre le
  chemin PostGIS : `/canopy` à 0,08–0,19 % de pixels d'écart (bords de
  polygones), positions d'arbres ≤ 0,2 m, `/sunlit` identique.
- **Aucune simplification, aucun élagage** (tippecanoe est exclu pour ça) :
  ces tuiles nourrissent un calcul, pas seulement un rendu.
- **Objets entiers, jamais découpés** : un objet à cheval sur plusieurs
  tuiles est écrit en entier dans chacune, le lecteur dédoublonne par `id`
  (convention héritée de `btiles.rs`).
- Tuiles gzip (déclaré dans l'en-tête PMTiles), tuiles vides absentes.

| Couche | Géométrie | Attributs |
|---|---|---|
| `buildings` | polygones (trous conservés) | `id` (osm_id), `name`, `height_m`, `height_from_osm` |
| `woods` | polygones (clairières conservées) | `id`, `name`, `height_m`, `height_from_osm`, `leaf_type` |
| `trees` | points | `id`, `height_m`, `crown_radius_m`, `leaf_type` |

## Génération et lecture

- Écriture : `bin/tilegen` (Rust) — extrait GeoJSONSeq → archive, encodeur
  MVT et writer PMTiles maison dans `vtiles.rs`, à côté du lecteur : une
  évolution de schéma ne peut pas en oublier un. Testé en aller-retour
  (`cargo test vtiles`).
- **Fusion** : `tilegen --merge base.pmtiles` réunit un extrait et une
  archive existante — les identifiants de tuile étant triés des deux côtés,
  c'est une jointure linéaire ; à identifiant OSM égal, le nouvel extrait
  l'emporte. C'est ce qui permet d'ajouter une région sans effacer les
  précédentes (`scripts/import-zone.sh` l'active tout seul).
- ⚠ **Conformité de l'encodeur** : nos décodeurs sont tolérants là où Mapbox
  est strict. Un `ClosePath` de compteur 0 (au lieu de 1, soit la commande
  15) a passé tous les aller-retours maison en rendant les tuiles illisibles
  par le SDK — d'où `closepath_command_is_spec_compliant`, qui inspecte les
  octets produits. Modifier l'encodeur = se relire contre la spec MVT 2.1,
  pas contre nos décodeurs.
- Lecture serveur : `vtiles.rs` — décodeurs PMTiles v3 et MVT maison
  (pas de dépendance protobuf). `helios-server/testdata/mini.pmtiles`
  (écrite par l'ancien générateur Python) reste la fixture de non-dérive :
  une implémentation indépendante doit rester lisible.
- Lecture client : `MVTDecoder.swift` (repo iOS) — troisième implémentation
  du même format.
- Toute évolution du schéma des couches = `vtiles.rs` (encode + decode),
  `MVTDecoder`/`VegetationTileRepository` côté iOS, et la fixture.

## Servir depuis R2 — Worker Cloudflare

Un **Worker Cloudflare** (`cloudflare/`, source protomaps vendorée) traduit
`/{name}/{z}/{x}/{y}.mvt` en lecture Range sur `{name}.pmtiles`, avec cache
au bord :

```
https://tiles.sunmap.tech/sunmap/14/8412/5844.mvt
https://tiles.sunmap.tech/sunmap.json          # TileJSON
```

Ce choix plutôt qu'un lecteur PMTiles côté client : un template
`{z}/{x}/{y}` se donne directement à un `VectorSource` Mapbox — c'est déjà
ainsi que le relief est chargé — alors que le SDK iOS ne sait pas ouvrir une
archive PMTiles sans gestionnaire de protocole custom. Le bucket reste
**privé**, le Worker y accédant par binding interne.

**Le client tape ce CDN en direct** (`TilesConfig`, repo iOS) : les tuiles ne
transitent plus par `helios-server`, qui n'a plus d'endpoint de tuiles du
tout. Le serveur lit la même archive de son côté, en local. Détails et
procédure de déploiement : `cloudflare/README.md`.

Upload : `scripts/r2-upload.py` (variables `R2_*`, cf.
`docs/import-zone.md`) ou rclone :

```
rclone copyto tiles/sunmap.pmtiles r2:sunmap-tiles/sunmap.pmtiles
```

Le fichier ne change qu'au réimport : `Cache-Control` long côté bucket, et un
remplacement d'archive est atomique du point de vue du client (nouvel etag).

## Côté serveur

```
VECTOR_TILES=tiles/sunmap.pmtiles   # helios-server/.env — OBLIGATOIRE
```

L'archive est l'unique chemin de données géométrique : sans la variable (ou
avec une archive illisible), le serveur refuse de démarrer — un serveur sans
géométrie classerait tout au soleil sans le dire. Le serveur ne sert
**aucune** tuile : le client passe par le CDN.

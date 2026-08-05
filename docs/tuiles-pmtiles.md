# `sunmap.pmtiles` — l'artefact vectoriel unique, servi depuis Cloudflare R2

Une archive PMTiles, tuiles **MVT** (Mapbox Vector Tiles), qui porte toute la
géométrie qui fait de l'ombre — plus un **aperçu dérivé** pour les zooms
lointains (§ « Aperçu de canopée »). Trois consommateurs, mêmes octets :

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
  devant le pixel DSM (~1,57 m). L'aperçu de canopée est la seule exception,
  et c'est une archive séparée. Les lecteurs sur/sous-échantillonnent
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

## Aperçu de canopée — `sunmap-veg.pmtiles`

Une **seconde archive**, dérivée de la première par `bin/vegoverview` :

    cargo run --release --bin vegoverview -- tiles/sunmap.pmtiles tiles/sunmap-veg.pmtiles

- **Couche `woods` seule**, niveaux **z12 et z13** (`vtiles::OVERVIEW_ZOOMS`).
- Consommée par le seul **masque d'ombre du client**, sous z14. Le serveur ne
  l'ouvre jamais : la classification reste sur `sunmap.pmtiles`, donc
  l'invariant « même géométrie des deux côtés » est intact là où il compte.

**Pourquoi.** Le masque réclame la végétation de toute l'emprise visible : le
nombre de tuiles suit l'**aire**, pas le zoom de la carte. Une vue inclinée à
z12 demandait ~1 100 tuiles z14, huit à la fois. Une douzaine de tuiles z12
portent la même information utile à cette échelle.

**Ce qui est jeté, et pourquoi ça ne coûte rien.** Mesuré sur la tuile z14 du
centre de Paris (447 Ko) : `buildings` 316 Ko, `trees` 128 Ko, `woods` 3 Ko.

| Couche | Sort | Raison |
|---|---|---|
| `buildings` | jetée | le masque de canopée ne la lit pas — 71 % des octets pour rien |
| `trees` | jetée | sous z14 la DSM client tourne à 6-12 m/pixel, une couronne de 8 m y pèse un pixel ou moins ; ils reprennent la main à z14 |
| `woods` | gardée | telle quelle, sans simplification |

Regrouper les tuiles supprime au passage la duplication : un massif à cheval
sur seize tuiles z14 y est écrit seize fois, une seule dans sa tuile z12.
France entière : **429 Mo** (12 825 tuiles z12 + 49 786 z13), contre 1,8 Go
pour l'archive principale.

**Dérivée de l'archive, pas de l'extrait OSM** — un seul passage de lecture,
et surtout la garantie que l'aperçu porte *exactement* les emprises que le
serveur classe. Le générateur s'appuie sur une propriété de la courbe de
Hilbert : les descendants d'une tuile forment un intervalle contigu
d'identifiants, donc parcourir l'archive par `tile_id` croissant, c'est
parcourir les groupes z12 dans l'ordre — mémoire bornée à un groupe.

Vérifié à la génération France : parité **exacte** des identifiants de `woods`
entre l'aperçu et l'union de ses tuiles sources (Paris, Fontainebleau,
Bordeaux — 0 manquant, 0 en trop). L'écart résiduel est la quantification du
niveau, ≤ 1,2 m à z12, très en deçà du pixel de DSM (~12 m) qui le consomme.

## Servir depuis R2 — Worker Cloudflare

Un **Worker Cloudflare** (`cloudflare/`, source protomaps vendorée) traduit
`/{name}/{z}/{x}/{y}.mvt` en lecture Range sur `{name}.pmtiles`, avec cache
au bord :

```
https://tiles.sunmap.tech/sunmap/14/8412/5844.mvt
https://tiles.sunmap.tech/sunmap-veg/12/2074/1409.mvt
https://tiles.sunmap.tech/sunmap.json          # TileJSON
```

Le `{name}` de l'URL nomme l'archive : l'aperçu de canopée n'a demandé
**aucun changement au Worker**, seulement un second objet dans le bucket.

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

Les deux archives se poussent ensemble — `scripts/import-zone.sh --upload` le
fait. Pousser l'une sans l'autre laisse le client lire une canopée d'une autre
époque sous z14.

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

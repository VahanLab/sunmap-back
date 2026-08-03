# `sunmap.pmtiles` — l'artefact vectoriel unique, servi depuis Cloudflare R2

Une seule archive PMTiles, tuiles **MVT** (Mapbox Vector Tiles), qui porte
toute la géométrie qui fait de l'ombre. Trois consommateurs, mêmes octets :

1. **le serveur** (`helios-server/src/vtiles.rs`, `VECTOR_TILES=`) — la
   rasterisation DSM pour la classification soleil/ombre ;
2. **le masque Metal client** (à venir) — rasterisation GPU de la même
   géométrie ;
3. **l'affichage Mapbox** (à venir) — arbres 3D (`ModelLayer`), extrusions.

Ce qu'on voit est ce qui fait l'ombre, par construction. C'est cette archive
qui remplace les tables PostGIS `buildings`/`trees`/`woods` au runtime —
PostGIS n'est plus que la zone de transit de l'import
(cf. `docs/import-zone.md`, option `--purge`).

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

- Écriture : `scripts/build-pmtiles.py` (encodage via `mapbox-vector-tile`,
  container via `pmtiles`). `--selftest` fait l'aller-retour d'encodage sans
  base ; `--fixture` régénère `helios-server/testdata/mini.pmtiles`.
- Lecture serveur : `vtiles.rs` — décodeurs PMTiles v3 et MVT maison
  (~200 lignes, pas de dépendance protobuf), testés contre la fixture écrite
  par le générateur Python : deux implémentations indépendantes doivent se
  lire, c'est le test de non-dérive du format.
- Toute évolution du schéma des couches = les deux côtés + la fixture.

## Servir depuis R2

L'archive se sert **telle quelle** : un lecteur PMTiles fait des requêtes
HTTP Range (annuaire racine puis tuile), R2 les sert nativement, le CDN
Cloudflare cache les plages lues. Pas de Worker nécessaire pour un client
natif ; le Worker protomaps n'est utile que pour exposer des URLs `/z/x/y`
classiques (ce que Mapbox iOS peut préférer pour ses sources vectorielles).

Upload : `scripts/build-pmtiles.py --upload` (variables `R2_*`, cf.
`docs/import-zone.md`) ou rclone :

```
rclone copyto tiles/sunmap.pmtiles r2:sunmap-tiles/sunmap.pmtiles
```

Le fichier ne change qu'au réimport : `Cache-Control` long côté bucket, et un
remplacement d'archive est atomique du point de vue du client (nouvel etag).

## Côté serveur

```
VECTOR_TILES=tiles/sunmap.pmtiles   # helios-server/.env
```

Priorité des chemins de données : `VECTOR_TILES` > `BUILDINGS_TILES` (HBT,
legacy — bâtiments seuls) > PostGIS. Ne pas définir la variable = rollback
immédiat vers l'ancien chemin. Une archive illisible fait mourir le serveur
au démarrage plutôt que de retomber en silence sur un autre chemin.

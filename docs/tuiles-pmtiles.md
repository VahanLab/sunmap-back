# Tuiles statiques PMTiles sur Cloudflare R2

Objectif : sortir les tuiles bâtiments et canopée du chemin de requête du
serveur. Une archive PMTiles est un fichier unique, adressé par requêtes HTTP
Range — R2 le sert tel quel, sans Worker ni serveur de tuiles, et le CDN
Cloudflare cache les plages lues. Les données ne bougent qu'au réimport OSM :
du statique versionné convient parfaitement.

## Les deux archives

Générées par `scripts/build-pmtiles.py` (voir son en-tête pour le détail),
raster PNG 512 px, z12–15 — les mêmes bornes que le masque Metal client.
Une tuile sans donnée n'est pas écrite : l'absence (404 sur un `get`) signifie
« vide ».

| Archive | Contenu | Encodage PNG (RGB) |
|---|---|---|
| `canopy.pmtiles` | canopée (bois + arbres isolés) | identique à `GET /canopy/{z}/{x}/{y}` : R = sommet ×2, G = base ×2 (m au-dessus du sol, pas 0,5 m), B = classe (pas de 40 : silhouette × emprise/isolé) |
| `buildings.pmtiles` | hauteurs de bâtiments | hauteur en décimètres : `h_m = (R×256 + G) / 10` (plafond 6 553,5 m — le pas ×2 de la canopée plafonnerait à 127,5 m, trop bas pour La Défense) ; B = 255 si la hauteur vient d'un tag OSM, 0 si estimée (médiane locale) |

La rasterisation est un **port exact** de `canopy_tiles.rs` (scanline
pair-impair au centre du pixel, disques de couronne, mêmes règles de fusion) :
vérifié pixel à pixel contre le serveur sur Paris — zéro écart. Toute modif de
`canopy_tiles.rs` se répercute ici (et `--selftest` rejoue ses tests
unitaires).

## Pipeline (local, Île-de-France)

La source est PostGIS, jamais le PBF en direct : les règles tags → hauteur
vivent dans le Rust (`osm::building_from`, `osm::height_from_tags`) et ne
doivent pas être dupliquées.

```
scripts/osm-extract.sh                    # Geofabrik PBF → GeoJSON filtré
cargo run --release --bin import -- …     # → PostGIS (règles canoniques)

python3 -m venv .venv-tiles
.venv-tiles/bin/pip install numpy pillow "psycopg[binary]" pmtiles boto3
.venv-tiles/bin/python scripts/build-pmtiles.py --selftest   # sans base
.venv-tiles/bin/python scripts/build-pmtiles.py --out-dir tiles/
```

Options utiles : `--layer buildings|canopy`, `--bbox S,W,N,E` (défaut :
`ST_Extent` des tables), `--min-zoom/--max-zoom`, `--jobs`.

## Envoi vers R2

Au choix :

```
# rclone (remote `r2` configuré : type s3, provider Cloudflare)
rclone copyto tiles/canopy.pmtiles    r2:sunmap-tiles/canopy.pmtiles
rclone copyto tiles/buildings.pmtiles r2:sunmap-tiles/buildings.pmtiles

# ou le flag intégré (boto3, multipart géré)
export R2_ACCOUNT_ID=… R2_ACCESS_KEY_ID=… R2_SECRET_ACCESS_KEY=… R2_BUCKET=sunmap-tiles
.venv-tiles/bin/python scripts/build-pmtiles.py --out-dir tiles/ --upload
```

Côté bucket : exposer via un domaine public R2 (ou un domaine custom), activer
CORS si un client web doit lire, et laisser le cache CDN faire — le fichier ne
change qu'au réimport, un `Cache-Control` long est sain. Un remplacement
d'archive est atomique du point de vue du client (nouvel upload = nouvel etag).

## Côté client (pas encore branché)

Un lecteur PMTiles fait deux lectures Range (annuaire racine puis tuile) ;
l'annuaire se cache en mémoire, ensuite c'est une lecture par tuile. Pour
l'app iOS, un petit lecteur Swift du format v3 suffit (header 127 octets +
annuaires gzip) — ou exposer des URL `/z/x/y` classiques via le Worker
protomaps si on préfère ne rien changer au code de fetch existant.

# Importer une nouvelle zone (extrait PBF)

Procédure pour ajouter une zone géographique à SunMap : remplir PostGIS
(bâtiments, végétation, établissements, mobilier urbain) depuis un extrait
OSM, puis régénérer les tuiles. Relançable sans risque — l'import est un
upsert, réimporter une zone la rafraîchit.

## Prérequis

- `osmium` (`brew install osmium-tool`) ;
- Rust (le binaire `import` du workspace) ;
- PostgreSQL + PostGIS joignable via `DATABASE_URL`
  (défaut : `postgres://localhost/sunmap`, comme le serveur — les migrations
  s'appliquent au premier démarrage du serveur, pas à l'import) ;
- `python3` (le venv `.venv-tiles` se crée tout seul au premier lancement).

## La commande

Tout le pipeline tient en une commande, URL Geofabrik ou PBF déjà téléchargé :

```
scripts/import-zone.sh https://download.geofabrik.de/europe/france/ile-de-france-latest.osm.pbf
scripts/import-zone.sh pbf/ile-de-france-latest.osm.pbf --upload
```

Extraits Geofabrik : <https://download.geofabrik.de/europe/france.html>
(Île-de-France ~336 Mo, France entière ~5 Go).

Options :

- `--upload` : pousse `buildings.pmtiles` et `canopy.pmtiles` sur Cloudflare
  R2 après génération (variables `R2_*`, cf. plus bas) ;
- `--hbt` : régénère aussi `tiles/buildings.hbt`, les tuiles **internes** du
  serveur (`BUILDINGS_TILES`) — nécessaire pour un déploiement serveur
  (cf. `docs/deploiement-ovh.md` §4), inutile en local où le serveur lit
  PostGIS.

## Ce que fait chaque étape

1. **Téléchargement** (si URL) → `pbf/<zone>.osm.pbf`.
2. **`scripts/osm-extract.sh`** — osmium filtre le PBF sur les seuls objets
   utiles puis exporte en GeoJSONSeq : bâtiments (`building`,
   `building:part`, relations multipolygones), végétation (`natural=tree`,
   `wood`, `tree_row`, `scrub`, `landuse=forest`), établissements
   (`amenity=bar|pub|restaurant|cafe|fast_food|biergarten`), mobilier urbain
   (`amenity=bench`, `leisure=picnic_table`).
3. **`cargo run --release --bin import`** — remplit les tables `buildings`,
   `trees`, `woods`, `places`. C'est ICI que vivent les règles tags → hauteur
   (`osm::building_from`, `osm::height_from_tags`) : médiane locale pour les
   bâtiments sans tag, replis par type pour la végétation, déduction
   `leaf_type` depuis le genre. Ne jamais les dupliquer ailleurs.
4. **`scripts/build-pmtiles.py`** — génère `tiles/buildings.pmtiles` et
   `tiles/canopy.pmtiles` (raster PNG z12–15, formats détaillés dans
   `docs/tuiles-pmtiles.md`). ⚠ Les archives couvrent l'emprise **totale** de
   la base (`ST_Extent`), pas seulement la zone importée : elles sont
   globales, chaque import re-tuile tout et le remplacement sur R2 est
   atomique. Un `--selftest` (port conforme à `canopy_tiles.rs`) est joué
   avant chaque génération.

## Vérifier

```
psql sunmap -c "SELECT (SELECT count(*) FROM buildings) AS buildings,
                       (SELECT count(*) FROM trees)     AS trees,
                       (SELECT count(*) FROM woods)     AS woods,
                       (SELECT count(*) FROM places)    AS places;"
```

Ordres de grandeur Île-de-France : ~2,4 M de bâtiments, ~49 500 bancs et
~2 900 tables de pique-nique dans `places`. Côté tuiles, comparer une tuile
de l'archive à celle du serveur (`GET /canopy/{z}/{x}/{y}`) doit donner zéro
pixel d'écart.

## Cloudflare R2

Les archives sont servies statiquement depuis un bucket R2 (requêtes HTTP
Range, pas de serveur de tuiles — cf. `docs/tuiles-pmtiles.md`). Le script
d'upload lit quatre variables, depuis l'environnement ou
`helios-server/.env` (gitignoré, jamais dans le dépôt) :

```
R2_ACCOUNT_ID=…          # Account ID Cloudflare (page d'aperçu R2)
R2_ACCESS_KEY_ID=…       # jeton API R2, permission « Object Read & Write »
R2_SECRET_ACCESS_KEY=…   # affiché une seule fois à la création du jeton
R2_BUCKET=sunmap-tiles
```

Création côté dashboard Cloudflare : R2 Object Storage → créer le bucket
(région Europe) → « Manage R2 API Tokens » → jeton limité au bucket en
lecture/écriture. Pour que le client lise les tuiles : activer l'accès
public du bucket (URL `r2.dev` pour les tests ; domaine custom en
production — le `r2.dev` est bridé en débit et sans cache paramétrable).

## Limites connues

- `tilebuild` (option `--hbt`) tient tout en mémoire : ~500 Mo pour
  l'Île-de-France. Pour un très grand territoire, générer par sous-extraits.
- `build-pmtiles.py` interroge PostGIS tuile par tuile : compter ~30–60 min
  pour l'Île-de-France complète (z12–15) selon la machine. Les tuiles vides
  ne sont pas écrites.
- Un seul niveau de fraîcheur : les tuiles reflètent la base au moment de la
  génération. Après un `bin/ingest` (rafraîchissement Overpass d'une petite
  zone), relancer `build-pmtiles.py` si l'on veut les voir sur R2.

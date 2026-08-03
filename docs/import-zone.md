# Importer une nouvelle zone (extrait PBF)

Procédure pour ajouter une zone géographique à SunMap : remplir PostGIS
(bâtiments, végétation, établissements, mobilier urbain) depuis un extrait
OSM, puis régénérer `tiles/sunmap.pmtiles` — l'archive vectorielle unique qui
sert le calcul serveur comme le client (cf. `docs/tuiles-pmtiles.md`).
Relançable sans risque — l'import est un upsert, réimporter une zone la
rafraîchit.

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

- `--upload` : pousse `sunmap.pmtiles` sur Cloudflare R2 après génération
  (variables `R2_*`, cf. plus bas) ;
- `--purge` : vide `buildings`/`trees`/`woods` après génération — PostGIS ne
  garde que les lieux et les contributions. **Seulement si le serveur tourne
  avec `VECTOR_TILES=tiles/sunmap.pmtiles`** ; sans archive, plus de
  géométrie du tout. La régénération suivante repasse par un import.

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
4. **`scripts/build-pmtiles.py`** — génère `tiles/sunmap.pmtiles` (MVT z14,
   couches `buildings`/`woods`/`trees`, sans simplification — format détaillé
   dans `docs/tuiles-pmtiles.md`). ⚠ L'archive couvre l'emprise **totale** de
   la base (`ST_Extent`), pas seulement la zone importée : chaque import
   re-tuile tout, et le remplacement sur R2 est atomique. Un `--selftest`
   (aller-retour d'encodage MVT) est joué avant chaque génération.

Le serveur consomme l'archive via `VECTOR_TILES=tiles/sunmap.pmtiles`
(`helios-server/.env`) ; sans la variable il lit PostGIS comme avant —
c'est le rollback.

## Vérifier

```
psql sunmap -c "SELECT (SELECT count(*) FROM buildings) AS buildings,
                       (SELECT count(*) FROM trees)     AS trees,
                       (SELECT count(*) FROM woods)     AS woods,
                       (SELECT count(*) FROM places)    AS places;"
```

Ordres de grandeur Île-de-France : ~2,4 M de bâtiments, ~49 500 bancs et
~2 900 tables de pique-nique dans `places`. Côté archive : lancer le serveur
avec `VECTOR_TILES=` et comparer `/canopy/{z}/{x}/{y}`, `/trees` et quelques
`/sunlit` à l'instance PostGIS — attendu : mêmes classifications, ~0,1 % de
pixels d'écart en bord de polygone (quantification ~0,6 m), positions
d'arbres à ±0,2 m.

## Cloudflare R2

L'archive est servie statiquement depuis un bucket R2 (requêtes HTTP Range,
pas de serveur de tuiles). Le script d'upload lit quatre variables, depuis
l'environnement ou `helios-server/.env` (gitignoré, jamais dans le dépôt) :

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

- `build-pmtiles.py` interroge PostGIS tuile par tuile ; l'archive z14 de
  l'Île-de-France se génère en quelques minutes (~7 000 tuiles candidates).
- Après `--purge`, les endpoints et le calcul tournent sur l'archive ; toute
  régénération demande de réimporter la ou les zones voulues.
- `bin/ingest` (rafraîchissement Overpass d'une petite zone) écrit en base :
  relancer `build-pmtiles.py` ensuite pour voir le changement dans l'archive.
- Les tuiles internes HBT (`tilebuild`, `BUILDINGS_TILES`) restent
  disponibles mais sont dépréciées : `VECTOR_TILES` les remplace, bâtiments
  ET végétation.

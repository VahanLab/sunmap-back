# Importer une nouvelle zone (extrait PBF)

Procédure pour couvrir une zone géographique : générer l'archive vectorielle
`tiles/sunmap.pmtiles` (bâtiments, végétation — cf. `docs/tuiles-pmtiles.md`)
et charger les lieux (établissements, mobilier urbain) en base. **La
géométrie ne passe plus par PostgreSQL** : elle va de l'extrait OSM à
l'archive, directement.

## Prérequis

- `osmium` (`brew install osmium-tool`) ;
- Rust (binaires `tilegen` et `import` du workspace) ;
- PostgreSQL + PostGIS joignable via `DATABASE_URL` (défaut :
  `postgres://localhost/sunmap`) — **pour les lieux uniquement** :
  établissements, mobilier urbain, comptes, contributions. Les migrations
  s'appliquent au démarrage du serveur.

## La commande

Tout le pipeline tient en une commande, URL Geofabrik ou PBF déjà téléchargé :

```
scripts/import-zone.sh https://download.geofabrik.de/europe/france-latest.osm.pbf --upload
scripts/import-zone.sh pbf/ile-de-france-latest.osm.pbf
```

Extraits Geofabrik : <https://download.geofabrik.de/europe/france.html>
(Île-de-France ~330 Mo, France entière ~5 Go).

`--upload` : pousse l'archive sur Cloudflare R2 après génération
(`scripts/r2-upload.py`, variables `R2_*`, cf. plus bas).

⚠ **L'archive ne couvre que l'extrait donné.** Il n'y a plus de base
cumulative : pour couvrir plusieurs zones, partir d'un extrait qui les
contient toutes (ex. `france-latest`). L'import des lieux, lui, reste un
upsert cumulatif — relançable sans risque.

## Ce que fait chaque étape

1. **Téléchargement** (si URL) → `pbf/<zone>.osm.pbf`.
2. **`scripts/osm-extract.sh`** — osmium filtre le PBF sur les seuls objets
   utiles puis exporte en GeoJSONSeq : bâtiments (`building`,
   `building:part`, relations multipolygones), végétation (`natural=tree`,
   `wood`, `tree_row`, `scrub`, `landuse=forest`), établissements
   (`amenity=bar|pub|restaurant|cafe|fast_food|biergarten`), mobilier urbain
   (`amenity=bench`, `leisure=picnic_table`).
3. **`cargo run --release --bin tilegen`** — extrait → `tiles/sunmap.pmtiles`
   (MVT z14, couches `buildings`/`woods`/`trees`, sans simplification). Les
   règles tags → hauteur (`osm::building_from`, `osm::height_from_tags`,
   médiane locale des bâtiments non taggés) s'appliquent ICI, dans le Rust —
   ne jamais les dupliquer ailleurs. Aucune base de données. Tout tient en
   mémoire : l'Île-de-France passe large, la France entière demande ~20 Go
   de RAM.
4. **`cargo run --release --bin import`** — lieux vers PostgreSQL (upsert).

Le serveur consomme l'archive via `VECTOR_TILES=tiles/sunmap.pmtiles`
(`helios-server/.env`) — variable **obligatoire** : sans elle il refuse de
démarrer, un serveur sans géométrie classerait tout au soleil.

## Vérifier

```
psql sunmap -c "SELECT count(*) FROM places;"
```

Ordres de grandeur Île-de-France : ~84 500 lieux (dont ~49 500 bancs et
~2 900 tables de pique-nique). Côté archive : `tilegen` affiche les comptes
(~2,4 M de bâtiments IdF) ; lancer le serveur et comparer `/canopy/{z}/{x}/{y}`
et `/trees` à la version précédente — les tests `vtiles` (`cargo test`)
couvrent l'aller-retour encodeur/lecteur.

## Cloudflare R2

L'archive est servie statiquement depuis un bucket R2 (requêtes HTTP Range,
pas de serveur de tuiles). `scripts/r2-upload.py` lit quatre variables,
depuis l'environnement ou `helios-server/.env` (gitignoré, jamais dans le
dépôt) :

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

- `tilegen` tient l'extrait et l'archive en mémoire : pour un territoire
  plus grand que la France, générer par sous-extraits et fusionner (à
  outiller le jour venu).
- L'ingestion Overpass (`bin/ingest`) a disparu avec les tables
  géométriques : rafraîchir une zone = retélécharger son extrait PBF (les
  extraits Geofabrik sont quotidiens).
- Le client lit les tuiles via `GET /vtiles/{z}/{x}/{y}` du serveur tant
  qu'il n'est pas branché directement sur R2.

# Importer une nouvelle zone (extrait PBF)

Procédure pour couvrir une zone géographique : générer l'archive vectorielle
`tiles/sunmap.pmtiles` (bâtiments, végétation — cf. `docs/tuiles-pmtiles.md`)
et charger les lieux (établissements, mobilier urbain) en base. **La
géométrie ne passe plus par PostgreSQL** : elle va de l'extrait OSM à
l'archive, directement.

## Prérequis

- `osmium` (`brew install osmium-tool`) ;
- Rust (binaires `tilegen` et `import` du workspace) ;
- PostgreSQL + PostGIS joignable via `DATABASE_URL`, **sans valeur par
  défaut** — pour les lieux uniquement (établissements, mobilier urbain,
  comptes, contributions). Les migrations s'appliquent au démarrage du
  serveur.

### Quelle base ? Le geste doit être explicite

Il n'y a plus de repli silencieux : sans `DATABASE_URL`, l'import s'arrête.
`helios-server/.env` vise la base de **développement** ; `bin/import`
annonce l'hôte visé avant d'écrire quoi que ce soit.

Pour importer vers la **production** depuis un poste, passer le DSN le temps
d'une commande — une variable déjà présente dans l'environnement l'emporte
sur le `.env` :

```
DATABASE_URL="$(grep -h '^DATABASE_URL=' helios-server/.env.production | cut -d= -f2-)" \
  scripts/import-zone.sh <zone.osm.pbf>
```

`helios-server/.env.production` n'est chargé par **aucun** binaire : c'est un
aide-mémoire, gitignoré. Le DSN de production n'a rien à faire dans le `.env`
que lit le serveur de dev — un `cargo run` a déjà tenté d'y appliquer ses
migrations, dont un `DROP TABLE`, et seul un refus de droits l'a arrêté. Le
serveur refuse d'ailleurs désormais toute base non locale sans
`ALLOW_REMOTE_DB=1`, que docker-compose pose en production.

## La commande

Tout le pipeline tient en une commande, URL Geofabrik ou PBF déjà téléchargé :

```
scripts/import-zone.sh https://download.geofabrik.de/europe/france-latest.osm.pbf --upload
scripts/import-zone.sh pbf/ile-de-france-latest.osm.pbf
```

Extraits Geofabrik : <https://download.geofabrik.de/europe/france.html>
(Île-de-France ~330 Mo, France entière ~5 Go).

Options :

- `--upload` : pousse l'archive sur Cloudflare R2 (`scripts/r2-upload.py`)
  **puis purge le cache** (`scripts/cf-purge.py`) — cf. § Cloudflare ;
- `--replace` : repart d'une archive vide au lieu de fusionner.

**Les zones s'accumulent.** Chaque import fusionne l'extrait dans
`tiles/sunmap.pmtiles` s'il existe déjà : ajouter Rhône-Alpes ne fait pas
disparaître l'Île-de-France. À identifiant OSM égal, le nouvel extrait
l'emporte — c'est ce qui fait qu'un réimport applique bien les corrections
d'OSM. L'import des lieux est un upsert, de même esprit. Tout est donc
relançable sans risque.

## Ce que fait chaque étape

1. **Téléchargement** (si URL) → `pbf/<zone>.osm.pbf`.
2. **`scripts/osm-extract.sh`** — osmium filtre le PBF sur les seuls objets
   utiles puis exporte en GeoJSONSeq : bâtiments (`building`,
   `building:part`, relations multipolygones), végétation (`natural=tree`,
   `wood`, `tree_row`, `scrub`, `landuse=forest`), établissements
   (`amenity=bar|pub|restaurant|cafe|fast_food|biergarten`), mobilier urbain
   (`amenity=bench`, `leisure=picnic_table`).
3. **`cargo run --release --bin tilegen`** — extrait → `tiles/sunmap.pmtiles`
   (MVT z14, couches `buildings`/`woods`/`trees`, sans simplification),
   **fusionné** dans l'archive existante (`--merge`) : les tuiles que le
   nouvel extrait ne touche pas sont recopiées, les tuiles communes voient
   leurs objets réunis et dédoublonnés par identifiant OSM. Les
   règles tags → hauteur (`osm::building_from`, `osm::height_from_tags`,
   médiane locale des bâtiments non taggés) s'appliquent ICI, dans le Rust —
   ne jamais les dupliquer ailleurs. Aucune base de données, et une
   **mémoire bornée** (deux passes en flux + buckets disque par plage de
   tuiles Hilbert, blobs débordés dans un fichier temporaire) : le pic est
   le plus gros bucket, pas le pays — une VM de 4 Go passe la France.
4. **`cargo run --release --bin import`** — lieux vers PostgreSQL (upsert).

Le serveur consomme l'archive via `VECTOR_TILES=tiles/sunmap.pmtiles`
(`helios-server/.env`) — variable **obligatoire** : sans elle il refuse de
démarrer, un serveur sans géométrie classerait tout au soleil.

## Vérifier

```
psql sunmap -c "SELECT count(*) FROM places;"
```

Ordres de grandeur Île-de-France : ~84 500 lieux (dont ~49 500 bancs et
~2 900 tables de pique-nique).

Côté archive, `tilegen` affiche ses comptes et, en fusion, le nombre de
tuiles ayant reçu des objets de l'archive de base. **Le contrôle qui compte
après un ajout de zone : vérifier qu'une tuile de l'ANCIENNE zone répond
encore.** Une régression de fusion se voit là, pas dans les compteurs :

```
curl -s -o /dev/null -w "%{http_code}\n" \
  https://tiles.sunmap.tech/sunmap/14/8298/5636.mvt   # Paris
curl -s -o /dev/null -w "%{http_code}\n" \
  https://tiles.sunmap.tech/sunmap/14/8412/5844.mvt   # Lyon
```

**Conformité MVT** : `cargo test -p helios-server vtiles` couvre
l'aller-retour encodeur/lecteur *et* la conformité des commandes de
géométrie (`closepath_command_is_spec_compliant`). Ce dernier test existe
parce que nos décodeurs sont tolérants là où Mapbox ne l'est pas : un
`ClosePath` mal encodé passait inaperçu tout en rendant les tuiles
illisibles par le SDK. Toute modification de l'encodeur MVT
(`helios-server/src/vtiles.rs`) doit garder ces tests verts — et, en cas de
doute, se relire contre la spec 2.1 plutôt que contre nos propres
décodeurs.

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
lecture/écriture. Le bucket reste **privé** : les tuiles sont servies par le
Worker (`cloudflare/README.md`), qui y accède par binding interne.

### Purge du cache — indispensable après chaque upload

Le cache du Worker est indexé sur l'URL de la tuile et **ignore le
remplacement de l'archive**. Sans purge, les tuiles déjà servies restent
servies dans leur version précédente jusqu'à un jour. `--upload` enchaîne
donc automatiquement sur `scripts/cf-purge.py`, qui a besoin de deux
variables (mêmes emplacements que les `R2_*`) :

```
CLOUDFLARE_ZONE_ID=…      # dashboard → sunmap.tech → Overview, colonne de droite
CLOUDFLARE_PURGE_TOKEN=…  # jeton API dédié, cf. ci-dessous
```

Créer le jeton : dashboard → icône de profil → **API Tokens** → *Create
Token* → *Create Custom Token* :

- **Permissions** : `Zone` → `Cache Purge` → `Purge`
- **Zone Resources** : `Include` → `Specific zone` → `sunmap.tech`

Ni le jeton R2 (identifiants S3, aucun droit sur le cache) ni celui de
wrangler (OAuth, `zone (read)` seulement) ne conviennent — il en faut un
dédié. Variables absentes : le script le signale et laisse passer, l'import
n'échoue pas pour autant.

La purge est **totale sur la zone** : purger par nom d'hôte ou par préfixe
est réservé aux offres Enterprise. Sans conséquence ici, `sunmap.tech` et
`www` étant en « DNS only » (Vercel), les tuiles sont seules en cache.

## Limites connues

- `tilegen` est borné en mémoire (buckets disque), mais `osmium`
  (l'assemblage des aires de l'étape d'extraction) reste le passage le plus
  gourmand sur un très gros extrait — surveiller la RAM de la VM sur la
  France entière.
- L'ingestion Overpass (`bin/ingest`) a disparu avec les tables
  géométriques : rafraîchir une zone = retélécharger son extrait PBF (les
  extraits Geofabrik sont quotidiens).
- Le client lit les tuiles directement sur le CDN
  (`https://tiles.sunmap.tech/sunmap/{z}/{x}/{y}.mvt`) : le serveur n'en
  sert aucune, il lit sa propre copie locale de l'archive.
- La fusion relit l'archive de base tuile par tuile : le temps de
  génération croît avec la couverture déjà en place, pas seulement avec le
  nouvel extrait.

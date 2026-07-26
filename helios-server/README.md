# helios-server — API de query ensoleillement

Serveur axum répondant à la question : **« ce point GPS est-il au soleil à
l'instant t ? »** en tenant compte du relief **et des bâtiments** (ray
marching sur DSM, moteur `helios-core`).

## Mise en route

La géométrie OSM vit dans PostgreSQL/PostGIS, pas dans Overpass : l'API
publique met 5 à 20 s par bbox dense, répond 504 aux heures de pointe et
impose une politesse incompatible avec une requête par déplacement de carte.
Overpass ne sert plus qu'à remplir la base, hors du chemin de requête.

```bash
# 1. Base + schéma (PostGIS requis)
createdb sunmap
psql -d sunmap -f helios-server/schema.sql

# 2. Géométrie : extrait Geofabrik + osmium (cf. « Ingestion » ci-dessous)
curl -O https://download.geofabrik.de/europe/france/ile-de-france-latest.osm.pbf
scripts/osm-extract.sh ile-de-france-latest.osm.pbf extrait.geojsonl
cargo run --release --bin import -- extrait.geojsonl

# 3. Serveur
cargo run --release --bin helios-server   # port 8080
```

`DATABASE_URL` surcharge la connexion (défaut `postgres://localhost/sunmap`).

### Ingestion : deux chemins

**`import` (extrait PBF) — la voie normale.** Un extrait
[Geofabrik](https://download.geofabrik.de/europe/france.html) est filtré et
converti par `osmium`, puis chargé en base. Déterministe, reproductible,
versionnable, et sans solliciter de service tiers.

```bash
scripts/osm-extract.sh <entrée.osm.pbf> [sortie.geojsonl]
cargo run --release --bin import -- sortie.geojsonl
# ou en flux, sans fichier intermédiaire :
osmium export ... -f geojsonseq | cargo run --release --bin import
```

Mesuré sur l'emprise Paris (extrait Île-de-France, 336 Mo) : **45 s**
d'extraction osmium, **50 s** d'import, **181 Mo** de RSS crête, pour 357 498
emprises, 212 973 arbres et 21 336 établissements.

**`ingest` (Overpass par tuiles) — dépannage seulement.** Utile pour
rafraîchir une petite zone sans re-télécharger un extrait. Ne pas l'utiliser
au-delà d'une ville : l'ingestion de Paris demandait 192 requêtes réseau et
~45 min, avec 27 échecs au premier essai — et c'est un abus d'une ressource
gratuite partagée.

```bash
cargo run --release --bin ingest                 # Paris, les 3 couches
cargo run --release --bin ingest -- buildings    # une seule couche
cargo run --release --bin ingest -- --force      # réingère tout
INGEST_BBOX="48.5,2.0,49.0,2.7" cargo run --release --bin ingest
```

La zone est découpée en 8×8 tuiles, chaque tuile réussie étant tracée dans
`ingest_log` : une interruption se reprend en relançant la commande.

Les deux chemins écrivent les **mêmes identifiants** (`way/123`,
`relation/456`) et partagent les mêmes règles tags → hauteur, donc ils se
dédoublonnent : vérifié sur 206 103 emprises ingérées par Overpass puis
réécrites par `import`, 99,98 % ont été reconnues.

Penser à prendre une emprise plus large que la zone servie : un immeuble hors
zone porte quand même ombre à l'intérieur — au soleil rasant (5°), 20 m de
haut projettent 230 m.

## Capacités et limites

| Capacité | Détail |
|---|---|
| Relief | Tuiles DEM Mapterhorn z15 (~1,57 m/px à Paris), webp 512 px, encodage Terrarium — même source que le rendu iOS |
| Bâtiments | PostGIS. `way[building]`, `way[building:part]` et `relation[building]` (multipolygones), rasterisés en **vrai polygone** (scanline, règle pair-impair) — les cours intérieures restent creuses. Hauteur : tag `height`, sinon `building:levels × 3 m + 3 m` de toiture, sinon **médiane locale** du quartier |
| Marge de casters | Assemblage 3×3 tuiles autour du point → ombres portées venant jusqu'à ~1,2 km hors tuile centrale |
| Distance de recherche | 5 km max le long du rayon (recoupée avec l'altitude max de la grille — early exit) |
| Hauteur d'observateur | `observer_height` (m) : 0 = sol, 1.5 ≈ personne attablée (« tête au soleil, pieds à l'ombre ») |
| Position solaire | Algorithme NOAA (~0,01°), UTC |
| Traçage | Toute réponse « à l'ombre » nomme l'obstacle (`blocker`) : id OSM, hauteur, distance, de combien il dépasse le rayon |
| Cache | Tuiles DEM et emprises en RAM ; résultats `/places` par (bbox, tranche de 5 min, hauteur) |

**Limites actuelles :**

- **Résolution z15 = 1,57 m/px.** Limite pour les cours et les ruelles : une
  cour de 6 m ne fait que 4 pixels. z16 réglerait ça au prix de 4× les données.
- **Classification binaire au point** : une terrasse est un polygone,
  potentiellement mi-ombre/mi-soleil. Prochaine étape prévue : échantillonner
  3-5 points dans un buffer côté rue et renvoyer un pourcentage.
- **Déport aveugle à la rue.** Les nœuds OSM d'un bar sont posés sur le
  bâtiment et non sur sa terrasse (419 POI sur 422 dans une zone test) : le
  serveur ressort donc le point sur le sol libre le plus proche avant de
  calculer, et l'expose en `snapped_*`. Mais il ne sait pas de quel côté est
  la rue — pour une terrasse en angle il peut sortir du mauvais côté.
- **Arbres absents de la DSM** : la canopée ne porte pas ombre (backlog, cf.
  CLAUDE.md racine).
- Le DEM Mapterhorn n'est pas parfaitement bare-earth partout : quelques
  bloqueurs ressortent en `terrain` à très courte distance.
- Cache RAM non borné, pas de persistance. Pas d'auth ni de rate limiting :
  usage interne/dev.

## Endpoints

### `GET /sunlit`

Un seul point.

| Paramètre | Type | Obligatoire | Description |
|---|---|---|---|
| `lat` | f64 | oui | Latitude WGS84 (−85…85) |
| `lng` | f64 | oui | Longitude WGS84 (−180…180) |
| `t` | string | non | RFC3339 (`2026-07-25T18:30:00Z`) ou secondes Unix. Défaut : maintenant |
| `observer_height` | f64 | non | Hauteur de l'observateur en mètres. Défaut : 0 |

```bash
curl "http://localhost:8080/sunlit?lat=48.8611&lng=2.3493&t=2026-07-26T13:00:00Z&observer_height=1.5"
```

```json
{
  "sunlit": false,
  "elevation_m": 35.56,
  "sun_azimuth_deg": 208.7,
  "sun_elevation_deg": 57.9,
  "t_unix": 1785070800.0,
  "blocker": {
    "id": "way/206810022",
    "name": null,
    "height_m": 27.0,
    "height_from_osm": true,
    "lat": 48.860995, "lng": 2.349240,
    "distance_m": 12.57,
    "obstacle_elevation_m": 62.55,
    "ray_elevation_m": 57.10
  }
}
```

| Champ | Description |
|---|---|
| `sunlit` | `true` = au soleil, `false` = à l'ombre (ou nuit si `sun_elevation_deg` ≤ 0) |
| `elevation_m` | Altitude du point sur le **relief seul** (jamais un toit) |
| `blocker` | Absent si au soleil. `id` = `way/…`, `relation/…` ou `"terrain"` ; `height_from_osm: false` = hauteur estimée, première cause d'écart avec la réalité |

### `POST /sunlit/batch`

Plusieurs points au même instant. Corps JSON : `points` (`[{lat, lng}]`), `t`,
`observer_height`. Réponse : tableau au format du GET, dans l'ordre d'entrée.

### `GET /places`

Établissements de restauration et de boisson d'une bounding box, classés
soleil/ombre à l'instant t. Une requête par viewport ; le switch soleil/ombre
et les filtres de catégorie s'appliquent côté client, sans nouvel appel.

Catégories retenues (`osm::AMENITIES`) : `bar`, `pub`, `restaurant`, `cafe`,
`fast_food`, `biergarten`. **Aucun filtre sur `outdoor_seating`** : le tag
manque sur ~64 % des établissements parisiens, donc filtrer dessus en écartait
la majorité. Il est renvoyé en trois états — `true`, `false`, ou **absent de la
réponse** quand OSM ne renseigne rien. Un client ne doit rien affirmer dans ce
dernier cas : l'absence d'information n'est pas une absence de terrasse.

| Paramètre | Type | Obligatoire | Description |
|---|---|---|---|
| `bbox` | string | oui | `min_lon,min_lat,max_lon,max_lat` (max ~3 km de côté) |
| `t` | string | non | Défaut : maintenant |
| `observer_height` | f64 | non | Défaut : **1.5** |

```json
{
  "t_unix": 1785070800.0,
  "sun_azimuth_deg": 208.7, "sun_elevation_deg": 57.9,
  "count": 1181,
  "places": [
    {
      "id": "node/2298691508", "name": "Les Acrobates", "amenity": "bar",
      "outdoor_seating": true,
      "lat": 48.8575, "lng": 2.3496,
      "sunlit": true,
      "snapped_lat": 48.85750, "snapped_lng": 2.34947, "snapped_distance_m": 6.3,
      "elevation_m": 35.6,
      "website": null, "phone": null, "opening_hours": null,
      "cuisine": null, "wikidata": null
    }
  ]
}
```

Les champs `snapped_*` sont absents si le nœud OSM était déjà sur du sol
libre. Sinon ils donnent le point **réellement classé** : c'est ce qui permet
au client d'afficher le déport et de distinguer « le calcul est faux » de « le
calcul porte sur un autre endroit ».

### `POST /places/terrace`

Terrasse signalée par un utilisateur : sa présence, et sa position si elle a été
pointée sur la carte. Cette position prime sur tout ce que le serveur peut
déduire — OSM place le nœud d'un bar sur son bâtiment, et le repli automatique
le ressort au jugé sur le sol libre le plus proche, sans savoir de quel côté est
la rue.

L'identifiant est dans le corps et non dans le chemin : il contient une barre
oblique (`node/123`), qui casserait le routage.

| Champ | Type | Obligatoire | Description |
|---|---|---|---|
| `osm_id` | string | oui | `node/123`, `way/456`… doit exister dans `places` |
| `has_terrace` | bool | oui | Prime sur le tag OSM, y compris pour le contredire |
| `lat`, `lng` | f64 | non | Position de la terrasse. Ignorées si `has_terrace` est faux |

```bash
curl -X POST http://localhost:8080/places/terrace \
  -H 'Content-Type: application/json' \
  -d '{"osm_id":"node/250657148","has_terrace":true,"lat":48.86476,"lng":2.34122}'
```

```json
{"osm_id": "node/250657148", "has_terrace": true, "located": true}
```

`404` si l'établissement est inconnu — sans quoi la table se remplirait de
lignes orphelines. La contribution est stockée dans `place_terraces`, **table
séparée de `places`** : `bin/import` fait un upsert sur `places` à chaque
réimport d'extrait OSM, ce qui effacerait toute colonne de contribution qu'on y
aurait ajoutée.

Les classifications en cache sont jetées, sinon la contribution resterait sans
effet visible.

### `GET /sun-hours`

Un point, une journée : heures au soleil et à l'ombre. Pensé pour l'appui long
sur la carte — statut immédiat + timeline.

| Paramètre | Type | Obligatoire | Description |
|---|---|---|---|
| `lat`, `lng` | f64 | oui | Coordonnées |
| `t` | string | non | N'importe quel instant DANS la journée voulue |
| `observer_height` | f64 | non | Défaut **1.5** |
| `utc_offset_minutes` | i32 | non | Décalage du **lieu** par rapport à UTC (Paris en été : `120`). Détermine où tombent les bornes de la journée. Défaut `0` = journée UTC, presque jamais ce qu'on veut — **le client doit l'envoyer** |

```json
{
  "lat": 48.8566, "lng": 2.3522, "elevation_m": 34.9,
  "t_unix": 1784998800.0,
  "sunlit_now": true,
  "day_start_unix": 1784937600.0, "day_end_unix": 1785024000.0,
  "total_sunlit_minutes": 355, "total_shadow_minutes": 1085,
  "intervals": [
    {"start_unix": 1784937600.0, "end_unix": 1784980500.0, "sunlit": false}
  ]
}
```

`blocker_now` apparaît quand le point est à l'ombre à `t_unix`, au format de
`/sunlit`. `intervals` : segments contigus (échantillonnage 5 min, regroupé)
en secondes Unix UTC — le client formate dans le fuseau que la réponse
renvoie en `utc_offset_minutes`, et surtout pas dans celui de l'appareil : ce
sont les deux seuls dans lesquels les bornes tombent bien sur minuit.

**Limite** : le décalage vient du client, donc en pratique du téléphone, et non
du lieu regardé. Correct tant qu'on consulte sa propre ville — le cas dominant.
Une vraie résolution lat/lng → fuseau demanderait une base de fuseaux côté
serveur (crate `tzf-rs` par exemple).

### `GET /trees`

Arbres OSM (`natural=tree`) de la bbox — géométrie seule, aucun calcul
soleil/ombre (le rendu et l'extrusion restent côté client, et la canopée n'est
pas encore dans la DSM).

| Paramètre | Type | Obligatoire | Description |
|---|---|---|---|
| `bbox` | string | oui | `min_lon,min_lat,max_lon,max_lat` |

```json
{"count": 1284, "trees": [{"lat": 48.8571, "lng": 2.3494, "height_m": 12.0, "crown_radius_m": 3.6}]}
```

### `GET /debug/ray`

Profil de la DSM le long du rayon solaire, pas à pas : altitude du terrain +
bâtiments contre altitude du rayon, avec le bâtiment occupant chaque cellule.
Mêmes paramètres que `/sunlit`. C'est l'outil pour comprendre *pourquoi* un
point est classé comme il l'est — bâtiment manquant, trop bas, mal placé.

```json
{
  "sun_azimuth_deg": 107.7, "sun_elevation_deg": 40.1,
  "ground_m": 35.56, "observer_m": 37.06, "meters_per_pixel": 1.57,
  "buildings_loaded": 10826,
  "sunlit": true,
  "steps": [
    {"distance_m": 39.3, "lat": 48.8610, "lng": 2.3497,
     "dsm_m": 56.9, "ray_m": 70.2,
     "building": "way/55489269", "building_height_m": 21.0, "blocks": false}
  ]
}
```

### Erreurs

| Code | Cause |
|---|---|
| `400` | `lat`/`lng` hors bornes, `bbox` invalide ou trop grande, `t` invalide |
| `500` | Requête PostGIS en erreur (base absente ou non ingérée ?) |
| `502` | Tuile Mapterhorn inaccessible ou indécodable |

## Schéma PostGIS

Trois tables, une par couche, plus `ingest_log` pour la reprise. Géométries en
EPSG:4326, index GIST sur chacune. Cf. `schema.sql`, commenté.

| Table | Géométrie | Contenu |
|---|---|---|
| `buildings` | `MultiPolygon` | Emprises + hauteur + provenance de la hauteur. Les relations gardent leurs anneaux intérieurs (cours) |
| `trees` | `Point` | Hauteur et rayon de couronne |
| `places` | `Point` | Établissements et leurs tags, position OSM **non corrigée** — le déport côté rue dépend de la DSM et se calcule au runtime |
| `place_terraces` | `Point` | Terrasses signalées par les utilisateurs. Séparée de `places` pour survivre aux réimports |

## Roadmap (cf. CLAUDE.md racine)

- Échantillonnage multi-points par terrasse → pourcentage d'ensoleillement
  plutôt qu'un booléen, et déport orienté vers la rue
- Canopée dans la DSM (Meta/WRI CHM + IGN MNS), atténuation saisonnière
- Tuiles d'ombre raster `GET /shadow/{z}/{x}/{y}.png?t=` si besoin web
- Cache CDN clé `(z,x,y,jour,tranche 5-10 min)`

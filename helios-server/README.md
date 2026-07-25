# helios-server — API de query ensoleillement

Serveur axum répondant à la question : **« ce point GPS est-il au soleil à
l'instant t ? »** en tenant compte du relief **et des bâtiments** (ray
marching sur DSM, moteur `helios-core`).

## Lancer

```bash
cargo run -p helios-server            # debug, port 8080
cargo run -p helios-server --release  # prod
```

## Capacités et limites

| Capacité | Détail |
|---|---|
| Relief | Tuiles DEM Mapterhorn z15 (~2,4 m/px à 45° lat), webp 512 px, encodage Terrarium |
| Bâtiments | Overpass `building=*`, rasterisés en emprise **rectangulaire** (bbox de l'empreinte, pas le polygone réel — cf. limites) dans la même DSM que le relief. Hauteur : tag `height`, sinon `building:levels × 3 m`, sinon 9 m par défaut |
| Marge de casters | Assemblage 3×3 tuiles autour du point → les ombres portées venant jusqu'à ~1,2 km hors de la tuile centrale sont prises en compte |
| Distance de recherche | 5 km max le long du rayon (borne recoupée avec l'altitude max de la grille — early exit) |
| Hauteur d'observateur | Paramètre `observer_height` (m) : 0 = sol, 1.5 ≈ personne attablée (cas « tête au soleil, pieds à l'ombre ») |
| Position solaire | Algorithme NOAA (~0,01°), UTC |
| Cache | Tuiles décodées gardées en RAM (clé z/x/y) — première requête sur une zone ≈ 1-2 s (fetch 9 tuiles), suivantes < 50 ms |
| Couverture | Monde (bounds Mapterhorn ±85°) |

**Limites actuelles :**
- **Bâtiments approximés en rectangle** : chaque bâtiment est rasterisé sur
  la bbox de son empreinte, pas son polygone réel — sous-estime les coins
  d'un bâtiment en L ou aux formes très irrégulières (l'ombre portée déborde
  légèrement moins ou plus que la réalité selon la forme). Prochaine étape :
  vraie rasterisation polygone (scanline).
- **Arbres non inclus** : canopée absente de la DSM (backlog, cf. CLAUDE.md
  racine — atténuation saisonnière, Meta/WRI CHM).
- Deux requêtes Overpass par appel (POI + bâtiments) : latence à froid plus
  élevée qu'avant (cache par bbox partagé, donc amorti sur les requêtes
  suivantes).
- Ombres venant de plus de ~1,2 km hors tuile centrale ignorées (soleil très
  rasant en haute montagne : rare mais possible).
- Cache RAM non borné, pas de persistance (redémarrage = cache vide).
- Pas d'auth, pas de rate limiting : usage interne/dev.

## Endpoints

### `GET /sunlit`

Un seul point.

| Paramètre | Type | Obligatoire | Description |
|---|---|---|---|
| `lat` | f64 | oui | Latitude WGS84 (−85…85) |
| `lng` | f64 | oui | Longitude WGS84 (−180…180) |
| `t` | string | non | Instant : RFC3339 (`2026-07-25T18:30:00Z`) ou secondes Unix (`1785004200`). Défaut : maintenant |
| `observer_height` | f64 | non | Hauteur de l'observateur en mètres. Défaut : 0 |

```bash
curl "http://localhost:8080/sunlit?lat=45.9237&lng=6.8694&t=2026-07-25T18:30:00Z"
```

Réponse `200` :

```json
{
  "sunlit": false,
  "elevation_m": 1035.6,
  "sun_azimuth_deg": 292.5,
  "sun_elevation_deg": 5.5,
  "t_unix": 1785004200.0
}
```

| Champ | Description |
|---|---|
| `sunlit` | `true` = au soleil, `false` = à l'ombre (ou nuit si `sun_elevation_deg` ≤ 0) |
| `elevation_m` | Altitude du point d'après la DSM |
| `sun_azimuth_deg` | Azimut du soleil, degrés depuis le nord, sens horaire |
| `sun_elevation_deg` | Élévation du soleil au-dessus de l'horizon (négatif = nuit) |
| `t_unix` | Instant évalué, secondes Unix |

### `POST /sunlit/batch`

Plusieurs points au même instant (classification de terrasses). Corps JSON :

| Champ | Type | Obligatoire | Description |
|---|---|---|---|
| `points` | `[{lat, lng}]` | oui | Liste de points |
| `t` | string | non | Comme pour GET |
| `observer_height` | f64 | non | Commun à tous les points |

```bash
curl -X POST http://localhost:8080/sunlit/batch \
  -H "Content-Type: application/json" \
  -d '{
    "points": [
      {"lat": 45.9237, "lng": 6.8694},
      {"lat": 45.8790, "lng": 6.8878}
    ],
    "t": "2026-07-25T18:30:00Z",
    "observer_height": 1.5
  }'
```

Réponse `200` : tableau de réponses au même format que le GET, dans l'ordre
des points d'entrée.

### `GET /terraces`

Bars/restaurants/cafés avec terrasse (OSM `outdoor_seating=yes` via Overpass)
dans une bounding box, classés soleil/ombre à l'instant t. Pensé pour le
front : une requête par viewport, le switch soleil/ombre filtre côté client
sur le champ `sunlit`.

| Paramètre | Type | Obligatoire | Description |
|---|---|---|---|
| `bbox` | string | oui | `min_lon,min_lat,max_lon,max_lat` (max ~3 km de côté) |
| `t` | string | non | RFC3339 ou secondes Unix. Défaut : maintenant |
| `observer_height` | f64 | non | Défaut : **1.5** (personne attablée) |

```bash
curl "http://localhost:8080/terraces?bbox=6.860,45.917,6.880,45.930&t=2026-07-25T18:30:00Z"
```

Réponse `200` :

```json
{
  "t_unix": 1785004200.0,
  "sun_azimuth_deg": 292.5,
  "sun_elevation_deg": 5.5,
  "count": 16,
  "terraces": [
    {
      "id": "node/2298691508",
      "name": "Le Chamonix",
      "amenity": "bar",
      "lat": 45.9235,
      "lng": 6.8697,
      "sunlit": false,
      "elevation_m": 1037.0
    }
  ]
}
```

Notes :
- Source POI : Overpass (`amenity=bar|restaurant|cafe` + `outdoor_seating=yes`),
  centroïde pour les bâtiments (ways). Cache mémoire par bbox — première
  requête sur une zone : 1-5 s (Overpass), suivantes : tuiles + POI en cache.
- **Classification binaire au centroïde** (limite POC) : une terrasse est un
  polygone, potentiellement mi-ombre/mi-soleil. Prochaine étape :
  échantillonner 3-5 points dans un buffer côté rue et renvoyer un
  pourcentage d'ensoleillement.
- Un POI dont les coordonnées OSM tombent à l'intérieur d'un bâtiment
  (défaut de saisie fréquent) peut ressortir « à l'ombre toute la journée » —
  piégé sous son propre toit stampé. L'altitude de l'observateur est bien
  prise sur le relief seul (pas de faux `sunlit: true`), mais le test
  d'obstruction utilise encore la DSM avec bâtiments y compris le sien.

### `GET /sun-hours`

Un point, une journée : les heures au soleil et à l'ombre. Pensé pour
l'appui long sur la carte côté app — statut immédiat + timeline complète.

| Paramètre | Type | Obligatoire | Description |
|---|---|---|---|
| `lat`, `lng` | f64 | oui | Coordonnées du point |
| `t` | string | non | N'importe quel instant DANS la journée voulue (RFC3339 ou secondes Unix). La journée = jour calendaire **UTC** contenant `t`. Défaut : maintenant |
| `observer_height` | f64 | non | Défaut **1.5** (terrasse/personne assise) |

```bash
curl "http://localhost:8080/sun-hours?lat=48.8566&lng=2.3522&t=2026-07-25T17:00:00Z"
```

Réponse `200` :

```json
{
  "lat": 48.8566, "lng": 2.3522, "elevation_m": 34.9,
  "t_unix": 1784998800.0, "sunlit_now": true,
  "day_start_unix": 1784937600.0, "day_end_unix": 1785024000.0,
  "total_sunlit_minutes": 355, "total_shadow_minutes": 1085,
  "intervals": [
    {"start_unix": 1784937600.0, "end_unix": 1784980500.0, "sunlit": false},
    {"start_unix": 1784980500.0, "end_unix": 1785001500.0, "sunlit": true}
  ]
}
```

`intervals` : segments contigus (échantillonnage toutes les 5 min, regroupé),
`start_unix`/`end_unix` en secondes Unix UTC — le client formate en heure
locale. Même limites que `/sunlit` (relief + bâtiments, cf. ci-dessus).

### Erreurs

| Code | Cause |
|---|---|
| `400` | `lat`/`lng` hors bornes, `bbox` invalide ou trop grande, `t` invalide |
| `502` | Tuile Mapterhorn inaccessible/indécodable, ou Overpass en erreur |

## Roadmap (cf. CLAUDE.md racine)

- `GET /sun-hours?lat&lng&date` : cumuls (« au soleil jusqu'à 18h40 »)
- Stamping bâtiments (Overture/OSM) dans la DSM
- Tuiles d'ombre raster `GET /shadow/{z}/{x}/{y}.png?t=` si besoin web
- Cache CDN clé `(z,x,y,jour,tranche 5-10 min)`

# helios-server — API de query ensoleillement

Serveur axum répondant à la question : **« ce point GPS est-il au soleil à
l'instant t ? »** en tenant compte du relief (ray marching sur DSM, moteur
`helios-core`).

## Lancer

```bash
cargo run -p helios-server            # debug, port 8080
cargo run -p helios-server --release  # prod
```

## Capacités et limites

| Capacité | Détail |
|---|---|
| Relief | Tuiles DEM Mapterhorn z15 (~2,4 m/px à 45° lat), webp 512 px, encodage Terrarium |
| Marge de casters | Assemblage 3×3 tuiles autour du point → les ombres portées venant jusqu'à ~1,2 km hors de la tuile centrale sont prises en compte |
| Distance de recherche | 5 km max le long du rayon (borne recoupée avec l'altitude max de la grille — early exit) |
| Hauteur d'observateur | Paramètre `observer_height` (m) : 0 = sol, 1.5 ≈ personne attablée (cas « tête au soleil, pieds à l'ombre ») |
| Position solaire | Algorithme NOAA (~0,01°), UTC |
| Cache | Tuiles décodées gardées en RAM (clé z/x/y) — première requête sur une zone ≈ 1-2 s (fetch 9 tuiles), suivantes < 50 ms |
| Couverture | Monde (bounds Mapterhorn ±85°) |

**Limites actuelles :**
- **Bâtiments et arbres non inclus** : DSM = relief seul. Un point en ville à
  l'ombre d'un immeuble sera renvoyé `sunlit: true` si le terrain ne le masque
  pas. (Roadmap : stamping des emprises bâtiments dans la DSM.)
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

### Erreurs

| Code | Cause |
|---|---|
| `400` | `lat`/`lng` hors bornes, ou `t` invalide |
| `502` | Tuile Mapterhorn inaccessible ou indécodable |

## Roadmap (cf. CLAUDE.md racine)

- `GET /sun-hours?lat&lng&date` : cumuls (« au soleil jusqu'à 18h40 »)
- Stamping bâtiments (Overture/OSM) dans la DSM
- Tuiles d'ombre raster `GET /shadow/{z}/{x}/{y}.png?t=` si besoin web
- Cache CDN clé `(z,x,y,jour,tranche 5-10 min)`

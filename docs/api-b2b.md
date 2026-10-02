# API B2B — dossier de reprise

> **État au 2026-10-02 : page vitrine prête, backend non implémenté.**
> Décision de Karl : on publie la page de promotion et on **attend une première
> demande réelle** avant de construire l'API. Ce dossier rassemble tout ce qui a
> été trouvé, décidé et planifié, pour reprendre sans refaire l'exploration.

## Reprendre le projet — le jour où une demande arrive

1. **Qualifier la demande** en répondant au prospect : usage visé, zones (sont-elles
   couvertes ? cf. page couverture du site), volume estimé en points par mois,
   dates passées/futures ou seulement « maintenant ». Rappeler que l'appel se
   fait **de serveur à serveur** : depuis un navigateur, la clé serait lisible
   par n'importe quel visiteur.
2. **Relire les constats ci-dessous** (rien n'a bougé ? `git log` sur
   `helios-server/src/main.rs` depuis le 2026-10-02), puis ouvrir une session
   Claude Code sur ce dépôt : *« Implémente l'API B2B décrite dans
   `docs/api-b2b.md` »*. Le contrat et le plan ci-dessous ont été validés.
3. **Déployer** : merge sur `main` → workflow `deploy.yml`. La migration
   (`api_keys`, `api_usage`) s'applique toute seule au démarrage.
4. **Créer la clé** sur la VM et l'envoyer avec la documentation :
   `docker compose run --rm api apikey create --name "Acme" --email dev@acme.com --expires-in 30d`
   (la clé n'est affichée qu'une fois).
5. **Lancer les vérifications de fumée** (§ « Après le déploiement »).
6. **Suivre** la consommation (`apikey usage`), puis convertir l'essai en tarif.
7. **Site** : si le contrat a bougé pendant l'implémentation, mettre à jour
   l'exemple de `src/content/api.ts` (sunmap-web) ; retirer de son `AGENTS.md`
   la mention « API qui n'existe pas encore ».

## Le besoin, tel qu'exprimé

> Mettre l'API à disposition en B2B pour la vendre : à tout instant, récupérer
> via **une coordonnée GPS ou une liste (25 points maximum)** les données
> d'exposition au soleil de chacun des points — ce que fait déjà l'**appui long**
> sur la carte de l'app — avec une **authentification par jeton d'API**.
> Sur le site vitrine : une entrée « API », une page de promotion, et un bouton
> de demande de jeton qui rédige un courrier vers **karl@sunmap.tech**, objet
> « demande d'un accès à l'API », corps « Je souhaiterais essayer l'API
> SunMap » (un peu plus rédigé), en français ou en anglais selon la page.

### Décisions prises (2026-10-02)

| Sujet | Décision |
|---|---|
| Limites par clé | **Débit + expiration + comptage** : requêtes/minute par clé, date d'expiration facultative (clés d'essai), comptage quotidien requêtes/points. **Pas** de quota mensuel bloquant en v1. |
| Prix affiché | « Clé d'essai gratuite sur demande, puis un tarif adapté à votre volume. » |
| Entrée sur le site | En-tête + pied de page (pas d'encart sur l'accueil). |
| Portée immédiate | **Site seulement.** Backend différé à la première demande. |
| Fuseau horaire | Déduit des coordonnées côté serveur (proposé dans le plan, non contesté). |

## Ce qui est en ligne côté site (`sunmap-web`)

- Branche **`claude/clever-euler-yr79j8`** (deux commits ; vérifier si elle a
  été mergée dans `main`). Pages **`/fr/api`** et **`/en/api`**, redirection
  courte **`/api`**, lien « API » dans l'en-tête (masqué sous 360 px) et le
  pied de page.
- Textes : `src/content/api.ts` (le français fait foi). Page :
  `src/app/[locale]/api/page.tsx`. Courrier prérempli : `src/lib/mailto.ts`,
  destinataire `site.apiContactEmail`.
- Courrier généré :
  - FR — objet « Demande d’un accès à l’API SunMap », corps : *Bonjour, / Je
    souhaiterais essayer l’API SunMap et recevoir une clé d’essai. / Quelques
    mots sur mon projet : Entreprise ou organisation, Site web, Usage envisagé,
    Volume estimé (points par mois) / Merci d’avance,*
  - EN — objet « SunMap API access request », même corps traduit.
- **Promesses faites par la page — à tenir à l'implémentation :**
  1 point ou une liste jusqu'à 25 ; état soleil/ombre/nuit à l'instant choisi,
  passé, présent ou futur ; ce qui fait l'ombre (bâtiment, arbre, relief) avec
  hauteur et distance de l'obstacle ; journée au pas de 5 min et total
  d'heures de soleil ; prochain basculement (« au soleil jusqu'à 18 h 40 ») ;
  hauteur d'observateur réglable, 1,5 m par défaut ; heures locales ; « le même
  moteur que l'app » ; authentification par clé dans `Authorization` ;
  documentation fournie avec la clé d'essai.
- **L'exemple de requête/réponse de la page suit le contrat ci-dessous.** Son
  jeu de données (Paris, 48,8611 / 2,3493, 26 juillet 2026 15 h 00) : position
  du soleil (208,7° / 57,9°) et obstacle (bâtiment de 27 m à 12,6 m,
  `way/206810022`) sont de vraies sorties du moteur, la frise de la journée est
  illustrative.

## L'existant à exposer (lu le 2026-10-02)

L'appui long de l'app appelle **`GET /sun-hours`** — c'est ce calcul que l'API
B2B vend. Repères dans `helios-server/src/main.rs` (numéros de ligne au
2026-10-02) :

| Élément | Où | Rôle |
|---|---|---|
| `sun_hours` | l. ~1136 | Frise d'un point : 288 tranches de 5 min sur la journée locale, regroupées en intervalles `sunlit`/`shadow`/`night`, totaux en minutes, `state_now`, `blocker_now`, `shadow_source_now` |
| `day_bounds_local` | l. ~1124 | Bornes de la journée locale à partir de `utc_offset_minutes` fourni par le client |
| `assemble_point` | l. ~807 | DSM 3×3 tuiles z15 autour du point (~2,4 km) : relief Mapterhorn, bâtiments et canopée de l'archive vectorielle, grille `owner` |
| `PointCtx::classify_at` / `shadow_source_at` | l. ~891-1003 | Soleil/ombre à un instant + premier obstacle ; nature **dominante** de l'ombre sur tout le rayon |
| `describe_blocker` | l. ~768 | `Blocker` : id OSM, nom, hauteur (et si elle vient d'OSM), distance, altitudes |
| `BlockerKind` | l. ~884 | `arbre` < `bois` < `batiment` < `relief` (ordre = priorité) |
| `classify` / `sunlit_batch` | l. ~1005 / ~370 | Instant exact, un point ; lot séquentiel **sans limite de taille** |
| `parse_time` | l. ~2463 | `t` en RFC3339 ou secondes Unix, défaut maintenant |
| Clé globale `X-API-Key` | l. ~273 | Middleware sur tout le routeur (`API_TOKEN`) |

Coût d'un point : une DSM assemblée (9 tuiles DEM de 1 Mo, récupérées **en
série** si le cache est froid, + rasterisation des emprises et de la canopée),
puis 288 rayons pour la journée.

## Constats et pièges — à lire avant de coder

1. **La clé `X-API-Key` n'est pas une authentification.** C'est un filtre
   anti-scraping dont la valeur est embarquée dans l'app (`API_TOKEN`). Elle ne
   peut pas être donnée à un client B2B, et ne permet ni révocation ni
   comptage par client.
2. **Monter les routes B2B après le `.layer()` global.** axum n'applique un
   layer qu'aux routes déjà déclarées : `app.layer(global).merge(b2b)` laisse
   les routes `/v1` hors du filtre de l'app, avec leur propre middleware.
   `Authorization` est déjà utilisé par les jetons Firebase des routes de
   l'app, mais pas sur les mêmes routes : aucun conflit.
3. **Caches RAM non bornés** : `TileCache` (`dem.rs`, 1 Mo par tuile),
   `BuildingCache` et `PlacesResultCache` (`main.rs`). L'app consulte des
   zones proches ; un client B2B qui interroge des points dans toute la France
   ferait grossir la mémoire jusqu'à l'OOM. **À borner avant d'ouvrir.**
4. **Calcul CPU dans les handlers async**, sans `spawn_blocking`, sur une VM à
   2 vCPU partagée avec l'app : il faut un sémaphore B2B pour que l'app garde
   la main.
5. **`/sun-hours` n'est pas à l'instant exact** : `state_now` est l'état de la
   tranche de 5 min contenant `t`, calculé au début de la tranche.
   `day_bounds_local` suppose 24 h et un décalage fixe : **faux les jours de
   changement d'heure** (23 h ou 25 h).
6. **Le fuseau vient du client** (`utc_offset_minutes`, défaut 0 = UTC) —
   inadapté au B2B. Crates repérées : **`tzf-rs` 2.1.2** (coordonnées → nom
   IANA, données embarquées `tzf-dist`) et **`chrono-tz` 0.10.4** (décalage à
   un instant, heure d'été comprise).
7. **Couverture** : bâtiments et végétation n'existent que dans les zones
   importées ; ailleurs le calcul retombe au relief seul **sans le dire**. Et
   hors pays à LiDAR ouvert, `fetch_tile_or_ancestor` dégrade le relief (z13 en
   Italie) sur une base GLO-30 qui est un DSM (bâtiments fondus dedans).
8. **Infra** : `api.sunmap.tech` → Cloudflare (proxied) → Nginx Proxy Manager
   (20 r/s par IP, rafale 40, réponse 429) → `api:8080`. Rien à changer pour
   l'API B2B.
9. **Aucune nouvelle crate pour les clés** : `ring` 0.17 est déjà tiré par
   `jsonwebtoken` et `rustls` (`SystemRandom`, `digest::SHA256`) — l'ajouter en
   dépendance directe ne change pas `Cargo.lock`.
10. **Rétrocompatibilité** : `/sun-hours` et `/sunlit` servent l'app en
    production ; toute factorisation doit laisser leurs réponses identiques
    (cf. AGENTS.md, « Rétrocompatibilité de l'API serveur »).
11. **Sessions cloud** : crates.io et npm sont joignables, mais pas Mapterhorn
    ni `api.sunmap.tech` → pas de test de bout en bout depuis une session cloud.
    Un PostgreSQL 16 est installé, **sans PostGIS** : les migrations complètes
    n'y passent pas, seul le SQL des nouvelles tables y est vérifiable.
12. **Notion** : le serveur MCP `notion-sunmap` n'existe pas en session cloud
    (`.mcp.json` non versionné) et AGENTS.md interdit tout autre connecteur.
    Tâche à créer à la main dans « Développements » : *« API B2B v1 —
    implémenter docs/api-b2b.md »*.
13. **Référence de tests** au 2026-10-02 : 113 tests verts (14 `helios-core`,
    83 bibliothèque `helios-server`, 16 binaire).

## Contrat v1 prévu

Contrat **public et versionné**, distinct des routes de l'app : on peut faire
évoluer l'un sans casser l'autre. Champs en anglais. Une fois publié, `/v1`
n'accepte que des **ajouts** ; un changement cassant ouvre `/v2`.

### Routes

| Méthode | Route | Rôle |
|---|---|---|
| `GET` | `/v1/sun-exposure?lat=&lng=[&t=][&observer_height=][&timezone=]` | Un point ; renvoie l'objet résultat seul |
| `POST` | `/v1/sun-exposure` | 1 à 25 points ; renvoie `{ "results": [...] }` dans l'ordre d'entrée |
| `GET` | `/v1/openapi.yaml` | Spécification OpenAPI, publique, sans clé |

Authentification : **`Authorization: Bearer sm_live_…`**. Jamais en paramètre
d'URL (les URL finissent dans les journaux du proxy et de Cloudflare). Pas de
CORS : l'API ne s'appelle pas depuis un navigateur. Réponses en
`Cache-Control: no-store`.

### Requête `POST`

```json
{
  "t": "2026-07-26T15:00:00+02:00",
  "observer_height": 1.5,
  "timezone": "Europe/Paris",
  "points": [
    { "id": "terrasse-42", "lat": 48.8611, "lng": 2.3493 }
  ]
}
```

| Champ | Défaut | Règle |
|---|---|---|
| `t` | maintenant | RFC3339 ou secondes Unix (`parse_time`) ; vaut pour tous les points |
| `observer_height` | `1.5` | mètres, 0 à 100 |
| `timezone` | déduit de chaque point | nom IANA ; force le fuseau de tous les points |
| `points` | — | 1 à 25 ; `lat` ∈ [−85, 85], `lng` ∈ [−180, 180] ; `id` facultatif, renvoyé tel quel |

### Résultat par point

```json
{
  "id": "terrasse-42",
  "lat": 48.8611,
  "lng": 2.3493,
  "elevation_m": 35.6,
  "observer_height_m": 1.5,
  "timezone": "Europe/Paris",
  "instant": {
    "time": "2026-07-26T15:00:00+02:00",
    "state": "shadow",
    "shadow_source": "building",
    "obstacle": { "distance_m": 12.6, "height_m": 27, "osm_id": "way/206810022" },
    "sun": { "azimuth_deg": 208.7, "elevation_deg": 57.9 },
    "next_change": { "time": "2026-07-26T17:40:00+02:00", "state": "sunlit" }
  },
  "day": {
    "date": "2026-07-26",
    "start": "2026-07-26T00:00:00+02:00",
    "end": "2026-07-27T00:00:00+02:00",
    "sunlit_minutes": 355,
    "shadow_minutes": 565,
    "night_minutes": 520,
    "intervals": [
      { "start": "2026-07-26T00:00:00+02:00", "end": "2026-07-26T06:20:00+02:00", "state": "night" }
    ]
  },
  "coverage": { "buildings": true, "vegetation": true }
}
```

- **Heures** en ISO 8601 avec le décalage local du point.
- `instant` est calculé **à l'instant exact** `t` (comme `/sunlit`), pas sur la
  tranche de 5 min ; `day` reprend exactement la frise de `/sun-hours`.
- `state` : `sunlit` | `shadow` | `night`.
- `shadow_source` (absent au soleil et la nuit) : nature dominante,
  `BlockerKind` traduit — `arbre` → `tree`, `bois` → `woodland`, `batiment` →
  `building`, `relief` → `terrain`.
- `obstacle` (absent au soleil) : premier obstacle du rayon (`Blocker`) ;
  `height_m` et `osm_id` seulement pour un bâtiment.
- `next_change` : premier changement d'état après `t` **dans la journée
  locale**, `null` sinon (la veille de minuit, pas de lendemain en v1).
- `day` : journée calendaire locale de `t`, 23 h ou 25 h les jours de
  changement d'heure ; intervalles contigus au pas de 5 min.
- `coverage` : présence de bâtiments / de végétation dans la DSM autour du
  point. `false` signifie « calculé sur le relief seul » — honnête hors des
  zones importées.
- Point en erreur dans un lot : `{ "id", "lat", "lng", "error": { "code", "message" } }`
  à sa place ; les autres points sont servis normalement.

### Erreurs

Corps : `{ "error": { "code": "…", "message": "…" } }`. Le **code** est le
contrat (figé) ; le message est indicatif, en anglais.

| HTTP | `code` | Cas |
|---|---|---|
| 400 | `invalid_request` | JSON ou paramètres illisibles |
| 400 | `no_points` / `too_many_points` | 0 point, ou plus de 25 |
| 400 | `invalid_coordinates` | hors bornes (`GET` ; dans un lot, erreur par point) |
| 400 | `invalid_time` / `invalid_observer_height` / `invalid_timezone` | paramètre invalide |
| 401 | `missing_api_key` / `invalid_api_key` | en-tête absent, clé inconnue ou révoquée |
| 401 | `expired_api_key` | clé d'essai expirée — distincte pour que le client comprenne |
| 429 | `rate_limited` | débit de la clé dépassé, avec `Retry-After` |
| 502 | `elevation_data_unavailable` | tuile Mapterhorn injoignable (`GET` ; par point dans un lot) |
| 503 | `service_unavailable` | base des clés injoignable, ou file B2B saturée |
| 500 | `internal_error` | le reste |

En-têtes sur toute réponse authentifiée : `X-RateLimit-Limit`,
`X-RateLimit-Remaining`.

## Plan d'implémentation (validé)

### Base — migration `…_api_keys.up.sql`

```sql
-- Clés d'API B2B. La clé n'est jamais stockée : seulement son empreinte
-- SHA-256 (une clé aléatoire de ~190 bits n'a pas besoin d'un hachage lent)
-- et un préfixe lisible pour la reconnaître dans les journaux et la CLI.
CREATE TABLE api_keys (
    id                  bigserial PRIMARY KEY,
    name                text NOT NULL,              -- client / organisation
    contact_email       text,
    key_prefix          text NOT NULL UNIQUE,       -- ex. « sm_live_a1b2c3 »
    key_hash            bytea NOT NULL UNIQUE,      -- SHA-256 de la clé complète
    requests_per_minute integer NOT NULL DEFAULT 60,
    expires_at          timestamptz,                -- NULL = sans expiration
    revoked_at          timestamptz,
    note                text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    last_used_at        timestamptz
);

-- Consommation par clé et par jour (UTC) : de quoi facturer et surveiller.
CREATE TABLE api_usage (
    key_id   bigint NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    day      date   NOT NULL,
    requests bigint NOT NULL DEFAULT 0,
    points   bigint NOT NULL DEFAULT 0,
    errors   bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (key_id, day)
);
```

Le `.down.sql` supprime les deux tables. Comptage en tâche de fond après la
réponse (`tokio::spawn`), en une requête :

```sql
INSERT INTO api_usage (key_id, day, requests, points, errors)
VALUES ($1, (now() AT TIME ZONE 'UTC')::date, 1, $2, $3)
ON CONFLICT (key_id, day) DO UPDATE
SET requests = api_usage.requests + 1,
    points   = api_usage.points + EXCLUDED.points,
    errors   = api_usage.errors + EXCLUDED.errors;
```

### Code

| Fichier | Contenu |
|---|---|
| `src/api_keys.rs` (bibliothèque) | Format `sm_live_` + 32 caractères base62 tirés de `ring::rand::SystemRandom`, empreinte SHA-256, préfixe, lecture de `Authorization: Bearer`. Pur, testé. |
| `src/rate_limit.rs` (bibliothèque) | Token bucket en mémoire par clé (le débit vient de `api_keys`). Testé. |
| `src/timezone.rs` (bibliothèque) | Coordonnées → fuseau IANA (`tzf-rs`), bornes de la journée locale et décalage à un instant (`chrono-tz`), journées de 23/25 h. Testé. |
| `src/db.rs` | Requêtes clés (création, recherche par empreinte, liste, révocation) et consommation, au style des requêtes existantes. |
| `src/main.rs` | Extraction de la frise de `sun_hours` dans une fonction partagée qui prend **des bornes de journée explicites** (l'app garde `day_bounds_local`, la sortie de `/sun-hours` ne bouge pas) ; montage des routes `/v1` après le layer global. |
| `src/b2b.rs` (module **du binaire**, `mod b2b;` dans `main.rs`) | Routes, middleware d'authentification et de débit, validation, regroupement des points par tuile DEM centrale (même grille 3×3 → résultat identique, coût partagé), erreurs par point, types de réponse publics. Module enfant de `main.rs` : il voit `AppState`, `assemble_point`, `PointCtx` sans les rendre publics. |
| `src/bin/apikey.rs` | CLI : `create`, `list`, `revoke <préfixe>`, `usage [<préfixe>] [--month AAAA-MM]`. Même garde-fou de base distante que `import`. |
| `openapi/sun-exposure-v1.yaml` | Spécification servie par `GET /v1/openapi.yaml` (`include_str!`). |
| `Dockerfile` | Copier `apikey` dans l'image, à côté de `import` (la VM n'a pas de toolchain). |

Organisation : le **domaine** (calcul d'exposition, moteur existant), les
**règles applicatives** (validation, authentification, débit) dans des modules
purs testables, l'**infrastructure** (PostgreSQL, HTTP) en bordure.

### Garde-fous à poser en même temps

- **Sémaphore B2B** (`B2B_MAX_CONCURRENT`, défaut 2) : au-delà, les requêtes
  attendent ; après ~30 s, `503 service_unavailable`.
- **Bornes des caches** `TileCache` et `BuildingCache` (nombre d'entrées, purge
  des plus anciennes) — commit séparé, aucun effet tant que la borne n'est pas
  atteinte.
- Tuiles DEM d'un même point récupérées **en parallèle** plutôt qu'en série
  (cache froid : 9 allers-retours Mapterhorn par point aujourd'hui).
- Journaux : préfixe de clé, nombre de points, durée — **jamais la clé**.

### Documentation

- `helios-server/README.md` : section « API B2B (`/v1`) ».
- `AGENTS.md` : décision n° 9 (contrat `/v1` séparé, clés par client, CLI) et
  règle de rétrocompatibilité propre à `/v1` (ajouts seulement).
- `.env.example` : `B2B_MAX_CONCURRENT`, bornes de cache.
- Ce dossier : passer l'état en « implémenté », garder les constats.

### Tests

- Unitaires : format et empreinte des clés, lecture du Bearer, token bucket,
  fuseaux (Paris été/hiver, Londres, Madrid ; jour de passage à l'heure d'été
  = 23 h), frise sur DSM synthétique (mur → intervalles connus), prochain
  basculement, 0/25/26 points, regroupement par tuile, **forme JSON exacte**
  du contrat (instantané des noms de champs).
- **Non-régression `/sun-hours`** : même sortie avant/après la factorisation.
- Middleware testé seul : routeur `axum` minimal + magasin de clés en mémoire
  (trait), sans base ni tuiles — 401 / 401 expirée / 429 / 200.
- SQL : migration et requêtes sur un PostgreSQL local (sans PostGIS, tables
  B2B seules).

### Après le déploiement — vérifications de fumée

```bash
# Avant de déployer : garder une réponse de /sun-hours pour comparer après.
KEY=sm_live_…   # clé de test créée sur la VM (apikey create --name "test")

# Un point (le « + » du décalage s'écrit %2B dans une URL)
curl -s "https://api.sunmap.tech/v1/sun-exposure?lat=48.8611&lng=2.3493&t=2026-07-26T15:00:00%2B02:00" \
  -H "Authorization: Bearer $KEY" | jq .

# Lot de 3 points dont un hors bornes : 200, une erreur à la 3e place
curl -s https://api.sunmap.tech/v1/sun-exposure \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"points":[{"id":"paris","lat":48.8611,"lng":2.3493},{"id":"lyon","lat":45.764,"lng":4.8357},{"id":"faux","lat":91,"lng":0}]}' \
  | jq '.results[] | {id, state: .instant.state, error}'

# Sans clé → 401 missing_api_key ; 26 points → 400 too_many_points ;
# rafale au-delà du débit → 429 avec Retry-After.
curl -s -o /dev/null -w "%{http_code}\n" "https://api.sunmap.tech/v1/sun-exposure?lat=48.86&lng=2.35"

# L'app n'a rien vu : /sun-hours identique à la réponse gardée avant.
```

## Hors périmètre v1 (pistes)

- Quota mensuel bloquant, facturation automatique, portail de clés en
  libre-service.
- `/v1/places` — les terrasses au soleil d'une zone : l'objectif produit final
  (AGENTS.md), et le bitfield `sun_day` existe déjà.
- `next_change` au-delà de minuit, `t` par point dans un lot, pas de temps
  configurable.
- Échantillonnage en surface (pourcentage d'ensoleillement d'une terrasse
  plutôt qu'un point) — déjà en tête des « Prochaines étapes » d'AGENTS.md.

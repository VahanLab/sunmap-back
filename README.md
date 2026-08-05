# SunMap — backend et outillage

Trouver, sur une carte, les endroits **au soleil ou à l'ombre** — une terrasse
de café à 18 h, un banc à l'ombre en août. Les ombres sont calculées à la
demande, jamais précalculées : relief, bâtiments et végétation, à n'importe
quelle date et n'importe quelle heure.

Ce dépôt porte **le moteur, le serveur et l'outillage de données**. L'app iOS
et le site web vivent dans leurs propres dépôts (voir plus bas).

Vision produit, décisions d'architecture et historique des choix :
[AGENTS.md](AGENTS.md). Ce README-ci décrit l'agencement du dépôt et ce qui se
déploie.

## Répartition en une phrase

Le **serveur** est la vérité soleil/ombre : il assemble une DSM (terrain +
bâtiments + canopée), lance le ray marching et répond « ce lieu est-il au
soleil à t ? ». Le **client** ne fait que du rendu, et relit localement le
bitfield `sun_day` que le serveur lui a donné pour toute la journée — d'où un
slider fluide sans une requête par cran.

## Répertoires

| Répertoire | Rôle |
|---|---|
| [`helios-core/`](helios-core/) | Le moteur. Position solaire NOAA, DSM, ray marching. Crate Rust **zéro dépendance**, portable serveur / mobile / WASM. [README](helios-core/README.md) |
| [`helios-server/`](helios-server/) | L'API (axum) : `/places`, `/sunlit`, `/sun-hours`, tuiles de canopée, contributions, comptes, remontée OSM. Contient aussi les binaires `import` et `tilegen`. [README](helios-server/README.md) |
| [`cloudflare/`](cloudflare/) | Worker qui sert `sunmap.pmtiles` (sur R2) en `/{z}/{x}/{y}.mvt` pour le client. [README](cloudflare/README.md) |
| [`scripts/`](scripts/) | Pipeline de données : extraction osmium, import d'une zone, envoi R2, purge de cache, allègement de modèles 3D. |
| [`tools/asc/`](tools/asc/) | Fiche App Store (textes + captures, 5 langues) poussée par l'API App Store Connect. Node, zéro dépendance. [README](tools/asc/README.md) |
| [`docs/`](docs/) | Déploiement OVH, procédure d'import de zone, format PMTiles, état de l'art des données d'élévation. |
| `Dockerfile`, `docker-compose.yml` | Image de production et pile déployée sur la VM. |

### Présents localement, hors du dépôt

Ignorés par git, chacun pour une bonne raison :

| Chemin | Pourquoi |
|---|---|
| `ios/` | **Dépôt séparé** `sunmap-ios` — l'app SwiftUI/Mapbox. |
| `web/` | **Dépôt séparé** `sunmap-web` — le site Next.js. |
| `pbf/` | Extraits OSM téléchargés (des Go), régénérables depuis Geofabrik. |
| `tiles/` | `sunmap.pmtiles`, produit par `tilegen`, monté dans le conteneur. |
| `target/` | Artefacts Cargo. |
| `.env`, `helios-server/.env` | Secrets : base de données, OAuth OSM, tag d'image. |

## Mise en route

```bash
cargo test -p helios-core                   # 14 tests, sans réseau ni base
cargo run --release --bin helios-server     # API sur le port 8080
```

Le serveur exige `DATABASE_URL` (PostgreSQL avec PostGIS — la géométrie OSM a
quitté la base pour l'archive vectorielle, mais les lieux gardent un
`geometry(Point, 4326)`) et `VECTOR_TILES` pointant sur `tiles/sunmap.pmtiles`. Détail dans [helios-server/README.md](helios-server/README.md).

Ajouter une zone (bâtiments, végétation, établissements, mobilier) :

```bash
scripts/import-zone.sh https://download.geofabrik.de/europe/france-latest.osm.pbf --upload
```

Procédure complète : [docs/import-zone.md](docs/import-zone.md).

## Ce qui est dockerisé

Seul le **backend Rust**. L'app iOS, le site et le Worker Cloudflare ne passent
pas par Docker.

Le [`Dockerfile`](Dockerfile) construit depuis la **racine du dépôt** (le
workspace Cargo entier doit être visible, `helios-server` dépendant de
`helios-core` par chemin relatif) et produit une image `debian:bookworm-slim`
contenant **trois binaires** :

- `helios-server` — l'API ;
- `import` — chargement des lieux en base ;
- `tilegen` — extrait OSM → archive vectorielle.

Les deux outils voyagent avec le serveur pour que **les imports tournent sur la
VM** : la VM n'a pas de toolchain Rust, et les identifiants de la base managée
n'ont ainsi pas à quitter la production. Le processus est non-root, les
migrations SQL sont embarquées dans le binaire (`sqlx::migrate!`) et rejouées
au démarrage.

[`docker-compose.yml`](docker-compose.yml) décrit la pile de la VM OVH :

- `api` — l'image ci-dessus, tirée de GHCR, sans port publié sur l'hôte ;
- `proxy` — Nginx Proxy Manager (TLS Let's Encrypt, `80`/`443` ; l'admin `81`
  est lié à `127.0.0.1` uniquement, un port publié par Docker contournant ufw).

La base de données n'est **pas** dans la pile : PostgreSQL managé OVH, joint
par `DATABASE_URL`. Le drapeau `ALLOW_REMOTE_DB=1` y est posé délibérément —
sans lui le serveur refuse une base distante, garde-fou contre un `cargo run`
de poste de dev qui appliquerait ses migrations sur la production.

## Ce qui déclenche un déploiement

Un seul workflow : [`.github/workflows/deploy.yml`](.github/workflows/deploy.yml),
sur **push vers `main`**, et seulement si le push touche l'un de ces chemins :

```
Cargo.toml   Cargo.lock   helios-core/**   helios-server/**
Dockerfile   .dockerignore   docker-compose.yml
.github/workflows/deploy.yml
```

Autrement dit : une modification de doc, de script, de `tools/` ou de
`cloudflare/` **ne déploie rien** — un build Rust de plusieurs minutes suivi
d'un redémarrage de l'API ne se justifie pas. `workflow_dispatch` permet de
redéployer à la main (rollback, VM réinstallée).

Le déroulé :

1. **Build sur les runners GitHub**, pas sur la VM — un `cargo build --release`
   demande plus de RAM que la VM n'en a, et construire sur place couperait
   l'API pendant tout le build. Image poussée sur GHCR
   (`ghcr.io/vahanlab/sunmap-api`) en deux tags : `latest` et le SHA du commit.
2. **Déploiement SSH** sur la VM (environnement GitHub `production`, donc
   approbation manuelle possible) : `git pull --ff-only`, réécriture de
   `API_TAG` dans `.env`, `docker compose pull` puis `up -d --no-deps api`.
   Seul `api` redémarre — le proxy et ses certificats n'ont aucune raison de
   bouger.
3. **Vérification** : après 10 s, l'état du conteneur et son compteur de
   redémarrages. Si l'API n'est pas debout (échec de migration, base
   injoignable), les 100 dernières lignes de log partent dans le job et
   celui-ci échoue.
4. Purge des images de plus d'une semaine — jamais des volumes ni du bind mount
   `./tiles`.

Un rollback consiste à remettre l'ancien `API_TAG` dans le `.env` de la VM et
relancer `docker compose up -d api` : rien à reconstruire.

Secrets attendus (Settings → Secrets and variables → Actions) : `DEPLOY_HOST`,
`DEPLOY_USER`, `DEPLOY_SSH_KEY`, `DEPLOY_KNOWN_HOSTS`, plus la variable
optionnelle `DEPLOY_PATH`. Préparation de la VM :
[docs/deploiement-ovh.md](docs/deploiement-ovh.md).

### Déploiements manuels

- **Worker Cloudflare** : `cd cloudflare && npm run deploy` (Node 22 minimum).
  Aucun workflow ne le fait — le Worker change rarement.
- **Archive vectorielle** : `scripts/import-zone.sh … --upload` pousse
  `sunmap.pmtiles` sur R2, puis `scripts/cf-purge.py` vide le cache au bord.
- **Fiche App Store** : `node tools/asc/src/cli.js push`.

## Conventions

- Commentaires et documentation **en français**.
- `helios-core` reste **zéro dépendance** ; les dépendances vivent dans les
  binaires.
- `helios-core/src/sun.rs` et `SunPosition.swift` (dépôt iOS) sont des ports
  exacts l'un de l'autre : toute modification de l'un se répercute sur l'autre,
  tests compris.
- Grille DSM : `x` vers l'est, `y` vers le sud (ligne 0 = bord nord). Azimut en
  degrés depuis le nord, sens horaire ; élévation en degrés au-dessus de
  l'horizon.

# Déploiement sur VM OVH

Architecture cible :

```
Internet ──▶ Nginx Proxy Manager (443, Let's Encrypt) ──▶ api:8080 (helios-server)
                                                              │
                                                              ▼
                                       PostgreSQL managé OVH (PostGIS, TLS)
```

Trois briques : la VM (Docker Compose : API + proxy), la base managée OVH,
le domaine. Le fichier `docker-compose.yml` de la racine porte le tout.

## 1. Base PostgreSQL managée OVH

1. Panneau OVH → Databases → créer un cluster **PostgreSQL** (la plus petite
   offre suffit pour démarrer ; l'Île-de-France seule pèse ~10 Go, viser
   ~40 Go de stockage pour Europe + Amérique).
2. Activer l'extension **PostGIS** (onglet « Options avancées » ou par
   `CREATE EXTENSION postgis;` — la migration initiale le fait aussi si le
   rôle a le droit).
3. Créer la base `sunmap` et un utilisateur dédié.
4. **IP autorisées** : ajouter l'IP publique de la VM (la base managée
   refuse tout le reste par défaut — c'est le vrai pare-feu).
5. Récupérer l'URL de connexion ; `sslmode=require` obligatoire.

Le schéma s'applique tout seul : les migrations (`helios-server/migrations/`)
sont embarquées dans le binaire et rejouées à chaque démarrage — une base
vierge est initialisée au premier lancement de l'API.

## 2. VM

1. VM OVH (b2-7 ou équivalent : le ray marching est CPU, 2 vCPU suffisent au
   début, la RAM sert surtout aux caches de tuiles — 4 Go confortable).
2. Installer Docker + le plugin compose (`curl -fsSL https://get.docker.com | sh`).
3. Cloner le repo, copier `.env.example` en `.env`, remplir `DATABASE_URL`.
4. `docker compose up -d --build`.

Pare-feu OVH (ou `ufw`) : ouvrir 80 et 443 au monde, **restreindre 81**
(admin du proxy) à son IP, fermer tout le reste — 8080 n'est pas publié sur
l'hôte, l'API n'est joignable que via le proxy.

## 3. Domaine et proxy

1. DNS : un enregistrement `A` `api.<domaine>` → IP de la VM.
2. `http://<ip-vm>:81` → Nginx Proxy Manager (premier login
   `admin@example.com` / `changeme`, à changer immédiatement).
3. **Proxy Hosts → Add** : domaine `api.<domaine>`, forward
   `http://api:8080` (le nom de service compose résout dans le réseau
   interne). Activer « Block Common Exploits » et « Websockets » (inutile
   mais inoffensif).
4. Onglet **SSL** : « Request a new SSL Certificate » (Let's Encrypt),
   « Force SSL ». Le renouvellement est automatique.

Côté iOS : `HeliosServerConfig.baseURL` passe à `https://api.<domaine>` et
l'exception ATS HTTP de l'Info.plist peut disparaître (le trafic devient TLS).

## 4. Import des données OSM et tuilage des bâtiments

Depuis le tileset bâtiments (`btiles`, cf. ticket Notion « Tileset vectoriel
bâtiments »), les bâtiments ne restent PAS dans la base managée : ils passent
en fichier de tuiles sur le disque de la VM, servi par le conteneur. La base
managée ne garde que places, arbres, bois et contributions (~5-10 Go pour le
lot 1 au lieu de 70-85).

Le pipeline par pays, une fois le PBF Geofabrik téléchargé sur la VM :

```bash
# 1. Extrait PBF → GeoJSONL (osmium requis sur la VM, hors Docker).
scripts/osm-extract.sh france-latest.osm.pbf france.geojsonl

# 2. Import complet vers la base managée (DATABASE_URL lu depuis .env).
#    C'est ici que les hauteurs sont résolues : tag OSM sinon médiane locale.
#    La table buildings sert d'étape de travail, elle sera purgée en 5.
docker compose run --rm api import france.geojsonl

# 3. Tuilage : lit la table buildings de la base et écrit le fichier HBT.
#    ./tiles doit être monté dans le conteneur (volume dans docker-compose).
#    Un fichier PAR PAYS : ne pas écraser celui d'un autre pays.
docker compose run --rm api tilebuild /tiles/france.hbt

# 4. Fusion dans le fichier servi (voir note fusion ci-dessous), puis
#    pointer le serveur dessus et redémarrer :
#    BUILDINGS_TILES=/tiles/buildings.hbt dans .env
docker compose up -d api

# 5. Une fois le serveur vérifié en mode tuiles (GET /places sur une zone du
#    pays), purger la table de travail pour libérer la base managée :
psql "$DATABASE_URL" -c "DELETE FROM buildings;"  # ou TRUNCATE si un seul pays en cours
```

Notes :
- **Ordre RAM** : `tilebuild` tient les tuiles en mémoire pendant la
  génération (~500 Mo pour l'Île-de-France). Pour l'Allemagne ou la France
  entière (~60 M de bâtiments), compter ~6-8 Go de RAM sur la VM — sinon
  passer par des sous-extraits régionaux Geofabrik (`france/ile-de-france`,
  `france/bretagne`…), un `.hbt` chacun.
- **Fusion multi-pays** : v1, un seul fichier est servi (`BUILDINGS_TILES`
  n'accepte qu'un chemin). Deux options : re-tuiler après chaque pays tant que
  `buildings` n'est pas purgée (le fichier couvre alors tout ce qui est en
  base), ou étendre `btiles` à une liste de fichiers — trivial côté lecteur
  (chercher la tuile dans chaque index), à faire quand le besoin arrive.
- **Rollback** : ne pas définir `BUILDINGS_TILES` → le serveur relit PostGIS.
  Ne purger `buildings` (étape 5) qu'une fois sûr.
- **Vérification de parité** (recommandée au premier pays) : requêter
  `/places` sur 2-3 bboxes avec et sans `BUILDINGS_TILES`, comparer
  classifications et `sun_day` — ils doivent être identiques (validé sur
  l'Île-de-France : parité exacte, chargement ~8× plus rapide).

Arbres, bois et établissements suivent le chemin base classique de l'étape 2 —
seuls les bâtiments sont tuilés.

## 5. Déploiement continu (GitHub Actions)

`.github/workflows/deploy.yml` déploie à chaque push sur `main` touchant le
serveur (`helios-*`, `Cargo.*`, `Dockerfile`, `docker-compose.yml`) :

```
push main ──▶ build image (runner GitHub) ──▶ ghcr.io/vahanlab/sunmap-api:<sha>
                                                      │
                                        ssh ──────────▼
                                   VM : git pull, compose pull, up -d api
```

**L'image est construite sur les runners, pas sur la VM.** Un
`cargo build --release` du workspace demande plus de RAM que la VM n'en a à
donner sans gêner l'API, et le build sur place couperait le service pendant
plusieurs minutes. La VM ne fait que tirer et redémarrer — quelques secondes.

### Préparer la VM

```bash
# 1. Clé SSH dédiée au déploiement, générée sur son poste (PAS sur la VM :
#    la clé privée ne doit exister que dans le secret GitHub).
ssh-keygen -t ed25519 -f ~/.ssh/sunmap-deploy -C "github-actions" -N ""

# 2. Autoriser la clé publique sur la VM, pour l'utilisateur de déploiement.
ssh-copy-id -i ~/.ssh/sunmap-deploy.pub <user>@<ip-vm>

# 3. Cet utilisateur doit pouvoir parler à Docker sans sudo.
ssh <user>@<ip-vm> "sudo usermod -aG docker <user>"

# 4. Empreinte de la VM, pour le secret DEPLOY_KNOWN_HOSTS.
ssh-keyscan <ip-vm>

# 5. Le repo doit être cloné sur la VM (le workflow y fait `git pull`),
#    avec un `.env` rempli.
ssh <user>@<ip-vm> "git clone https://github.com/VahanLab/sunmap-back.git sun-shadow"
```

Si le package GHCR reste **privé**, connecter une fois la VM au registre avec
un PAT `read:packages` — sinon `docker compose pull` échoue :

```bash
echo <PAT> | docker login ghcr.io -u <login-github> --password-stdin
```

Le rendre **public** (Packages → sunmap-api → Package settings → Change
visibility) évite ce jeton sur la VM. L'image ne contient que des binaires
compilés, aucun secret : `DATABASE_URL` arrive par l'environnement.

### Secrets GitHub

Settings → Secrets and variables → Actions :

| Nom | Contenu |
|---|---|
| `DEPLOY_HOST` | IP ou DNS de la VM |
| `DEPLOY_USER` | utilisateur SSH (groupe `docker`) |
| `DEPLOY_SSH_KEY` | contenu de `~/.ssh/sunmap-deploy` (clé **privée**) |
| `DEPLOY_KNOWN_HOSTS` | sortie de `ssh-keyscan <ip-vm>` |

Variable (onglet *Variables*, optionnelle) : `DEPLOY_PATH`, chemin du repo sur
la VM — défaut `sun-shadow`, relatif au home de `DEPLOY_USER`.

`DEPLOY_KNOWN_HOSTS` n'est pas du zèle : sans empreinte connue d'avance, il
faudrait accepter l'hôte à chaud, et le workflow livrerait ses commandes à
n'importe quel serveur répondant à cette IP.

Le job `deploy` tourne dans l'environnement GitHub `production` : le créer
(Settings → Environments) permet d'exiger une approbation manuelle avant
chaque mise en production, et de limiter la portée des secrets à `main`.

### Rollback

Le workflow écrit `API_TAG=<sha>` dans le `.env` de la VM. Revenir à une
version antérieure ne demande aucun build :

```bash
sed -i 's|^API_TAG=.*|API_TAG=<sha-precedent>|' .env
docker compose up -d --no-deps api
```

Les images de plus de 7 jours sont purgées à chaque déploiement
(`docker image prune`) : au-delà, retirer le filtre ou reconstruire depuis le
commit (`workflow_dispatch` sur le SHA voulu).

### Vérification

Après `up -d`, le workflow attend 10 s et échoue si le conteneur n'est pas
`running` ou a déjà redémarré — c'est la fenêtre où le serveur rejoue ses
migrations et ouvre la base, donc là où il tombe s'il doit tomber. Les 100
dernières lignes de log partent dans la sortie du job en cas d'échec.

Un vrai `GET /health` (sans base) serait plus franc que cette inspection de
conteneur : à ajouter quand l'endpoint existera.

## Rappels avant mise en production

- Retirer `/debug/ray` (tâche Notion `[MEP]`).
- Rate limiting (cf. discussion : token bucket par IP via le proxy ou en
  middleware axum, quotas par uid sur les contributions).
- L'admin NPM (port 81) ne doit jamais rester ouvert au monde.

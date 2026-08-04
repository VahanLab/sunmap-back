# Déploiement sur VM OVH

Architecture cible :

```
Internet ──▶ Cloudflare (proxy, Full strict) ──▶ Nginx Proxy Manager (443) ──▶ api:8080 (helios-server)
                                                                                   │
                                                                                   ▼
                                                            PostgreSQL managé OVH (PostGIS, TLS)
```

Le domaine `sunmap.tech` est déjà sur Cloudflare (nameservers délégués,
2026-08-03) : `sunmap.tech`/`www` restent en **DNS only** (ils pointent vers
Vercel, qui gère son propre SSL — les passer en proxy casse leur certificat,
cf. incident du 2026-08-03). `tiles.sunmap.tech` (archive R2) est déjà
branché. **`api.sunmap.tech` est en place** (2026-08-04, VM
`57.130.73.102`) — cf. § 3 ci-dessous.

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
3. Cloner le repo, copier `helios-server/.env.example` en
   `helios-server/.env`, remplir `DATABASE_URL` —
   et `OSM_CLIENT_ID` / `OSM_CLIENT_SECRET` si la remontée vers OpenStreetMap
   doit être active (sinon elle reste éteinte, pas cassée).
4. `docker compose up -d --build`.

**Deux fichiers `.env`, deux rôles**, aucun des deux versionné :

| Fichier | Contenu | Lu par |
|---|---|---|
| `helios-server/.env` | configuration de l'application : base, tuiles, OAuth OSM | le serveur (et le conteneur, via `env_file`) |
| `.env` (racine) | `API_IMAGE`, `API_TAG` | compose, pour ses `${…}` |

Le workflow de déploiement ne réécrit que le second : les secrets posés à la
main dans le premier y restent d'un déploiement à l'autre.

L'image étant construite en `--release`, elle vise **la production OSM**. Pour
un déploiement de recette, pointer explicitement le bac à sable dans
`helios-server/.env` :

```
OSM_API_BASE=https://master.apis.dev.openstreetmap.org
OSM_WEB_BASE=https://master.apis.dev.openstreetmap.org
```

Une application OAuth appartient à son instance : celle du bac à sable et celle
de production sont deux déclarations distinctes, avec deux `client_id`.

Pare-feu OVH (ou `ufw`) : ouvrir 22, 80 et 443, fermer tout le reste —
8080 n'est pas publié sur l'hôte, l'API n'est joignable que via le proxy.
**Attention : un port publié par Docker contourne `ufw`** (iptables, chaîne
DOCKER, traversée avant les règles hôte) — c'est pourquoi l'admin du proxy
(81) n'est pas publiée au monde mais liée à `127.0.0.1` dans le compose,
et s'atteint par tunnel SSH :

```bash
ssh -L 8181:localhost:81 <user>@<ip-vm>
# puis http://localhost:8181
```

## 3. Domaine et proxy — ✅ en place (2026-08-04)

Le SSL est géré par Cloudflare (Origin CA) plutôt que Let's Encrypt —
Nginx Proxy Manager sert toujours de reverse proxy interne, mais ne parle
plus au monde directement. Derrière le proxy orange, le client ne voit
jamais le certificat d'origine : Let's Encrypt n'apporterait qu'une
machinerie de renouvellement à 90 jours pour un certificat que seul
Cloudflare consulte, quand l'Origin CA se colle une fois pour 15 ans.

1. [x] **DNS Cloudflare** (pas OVH — le domaine y est déjà délégué,
   nameservers changés) : dashboard Cloudflare → `sunmap.tech` → DNS →
   Records → Add record : type `A`, name `api`, IPv4 = IP publique de la
   VM, Proxy status = **Proxied** (orange). Laisser `sunmap.tech`/`www` en
   DNS only (Vercel).
2. Admin Nginx Proxy Manager par tunnel SSH (cf. § 2) :
   `ssh -L 8181:localhost:81 <vm>` puis `http://localhost:8181` (premier
   login `admin@example.com` / `changeme`, à changer immédiatement).
3. **Proxy Hosts → Add** : domaine `api.sunmap.tech`, forward
   `http://api:8080` (le nom de service compose résout dans le réseau
   interne). Activer « Block Common Exploits » et « Websockets » (inutile
   mais inoffensif).
4. [x] **Certificat d'origine** : Cloudflare → SSL/TLS → Origin Server →
   Create Certificate (RSA, 15 ans, hostname `api.sunmap.tech`) → coller le
   certificat + la clé privée dans l'onglet **SSL** du Proxy Host NPM
   (« Custom » plutôt que « Request a new SSL Certificate » Let's Encrypt —
   Cloudflare valide ce certificat, pas une CA publique).
5. [x] Cloudflare → SSL/TLS → Overview → mode **Full (strict)** : sans ça,
   Cloudflare accepterait un certificat non vérifié à l'origine.
6. [x] **Allowlist Cloudflare** sur la VM : `scripts/cf-allowlist.sh`
   (installé en `/usr/local/sbin/`, service systemd `cf-allowlist` au boot
   + timer hebdomadaire de rafraîchissement des plages). N'accepte 80/443
   que depuis les plages Cloudflare — sinon l'IP de la VM, si elle fuite,
   permet de contourner le proxy. Les règles vivent dans la chaîne iptables
   `DOCKER-USER` : comme pour le port 81, `ufw` ne voit pas le trafic vers
   les ports publiés par Docker.
7. [x] **IP visiteur réelle** (`CF-Connecting-IP`) : fichier
   `/data/nginx/custom/server_proxy.conf` dans le volume NPM, contenant
   `real_ip_header CF-Connecting-IP;` seul. Pas au niveau http
   (`http_top.conf`) : le `nginx.conf` de NPM y définit déjà
   `real_ip_header X-Real-IP` (doublon = crash-loop) — au niveau server,
   la directive surcharge proprement, et les plages `set_real_ip_from`
   Cloudflare sont déjà dans l'`ip_ranges.conf` embarqué de NPM.

Côté iOS : `HeliosServerConfig.baseURL` passe à `https://api.sunmap.tech` et
l'exception ATS HTTP de l'Info.plist peut disparaître (le trafic devient TLS).

## 4. Import des données OSM et archive vectorielle — VM d'import dédiée

**Le pipeline d'import ne tourne PAS sur la VM applicative.** Une VM
dédiée (à commander au besoin, éphémère si l'on veut) porte osmium,
`bin/tilegen` et les jetons d'écriture : c'est elle qui télécharge le PBF
Geofabrik, génère `sunmap.pmtiles` et le pousse sur R2 + purge le cache
(`scripts/import-zone.sh <url> --upload`). Ses variables (`R2_*` en
« Object Read & Write », `CLOUDFLARE_ZONE_ID`, `CLOUDFLARE_PURGE_TOKEN`)
ne vivent que là — cf. `docs/import-zone.md`.

La VM applicative, elle, ne fait que **lire** : le serveur exige un fichier
local (`VECTOR_TILES`, obligatoire — la géométrie ne passe plus du tout par
la base managée, qui ne garde que lieux, comptes et contributions). Après
chaque import, rapatrier l'archive depuis R2 et redémarrer :

```bash
# Sur la VM applicative — R2_* (jeton « Object Read only ») dans
# helios-server/.env. Téléchargement atomique (.part puis rename).
python3 scripts/r2-download.py sunmap.pmtiles tiles/
docker compose restart api
```

Notes :
- **Séparation des jetons** : la VM applicative n'a qu'un jeton R2
  « Object Read only » limité au bucket. Écriture et purge restent sur la
  VM d'import — une VM applicative compromise ne peut pas corrompre les
  tuiles servies aux clients.
- **RAM (VM d'import)** : `tilegen` est borné en mémoire (flux + buckets
  disque, pic ~1 Go sur la France) ; c'est `osmium` (assemblage des aires)
  qui demande le plus — dimensionner la VM d'import pour lui.
- **Couverture** : l'archive ne couvre que l'extrait donné (plus de base
  cumulative) — prendre un extrait englobant toutes les zones voulues.
- **Client** : le Worker `tiles.sunmap.tech` sert les tuiles depuis le même
  bucket R2 (binding interne, bucket privé) — l'upload de la VM d'import
  alimente donc serveur ET client d'un coup.

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

### À faire sur GitHub (une fois)

Tout est dans `github.com/VahanLab/sunmap-back` → onglet **Settings**. Rien de
tout ceci n'est dans le repo : c'est de la configuration de compte, elle ne
part pas avec un `git clone` — d'où cette liste.

**1. Activer les Actions.** Settings → Actions → General → *Allow all actions
and reusable workflows* (le workflow tire `actions/checkout`, `docker/*`).

**2. Autoriser l'écriture sur les packages.** Settings → Actions → General →
*Workflow permissions*. Le workflow déclare lui-même `packages: write`, ce qui
suffit tant qu'une politique d'organisation ne plafonne pas le `GITHUB_TOKEN`.
Si le job `build` échoue en `denied: permission_access` au push de l'image,
c'est ici — passer sur *Read and write permissions*.

**3. Créer les secrets.** Settings → Secrets and variables → **Actions**,
onglet *Secrets* → *New repository secret* :

| Nom | Contenu | D'où il sort |
|---|---|---|
| `DEPLOY_HOST` | IP ou DNS de la VM | panneau OVH |
| `DEPLOY_USER` | utilisateur SSH, membre du groupe `docker` | choisi à l'étape « Préparer la VM » |
| `DEPLOY_SSH_KEY` | clé **privée**, contenu entier de `~/.ssh/sunmap-deploy` | `ssh-keygen` ci-dessus |
| `DEPLOY_KNOWN_HOSTS` | sortie brute de `ssh-keyscan <ip-vm>` | commande ci-dessus |

`DEPLOY_SSH_KEY` : coller le fichier en entier, lignes `-----BEGIN…` et
`-----END…` comprises, avec le retour à la ligne final. Une clé tronquée donne
un `Load key: error in libcrypto` sans autre explication.

`DEPLOY_KNOWN_HOSTS` n'est pas du zèle : sans empreinte connue d'avance, il
faudrait accepter l'hôte à chaud, et le workflow livrerait ses commandes à
n'importe quel serveur répondant à cette IP.

**4. Variable optionnelle.** Même écran, onglet *Variables* : `DEPLOY_PATH`,
chemin du repo cloné sur la VM. Défaut `sun-shadow`, relatif au home de
`DEPLOY_USER` — à définir seulement si le clone est ailleurs.

**5. Environnement `production`.** Settings → Environments → *New environment*,
nom exact `production`. GitHub le crée tout seul au premier run s'il manque,
mais le créer à la main permet d'y poser une *required reviewer* (approbation
manuelle avant chaque mise en production) et de limiter le déploiement à la
branche `main` (*Deployment branches*).

**6. Visibilité du package.** Le package GHCR n'existe qu'**après le premier
build réussi**. Une fois créé : page du repo → *Packages* → `sunmap-api` →
*Package settings*. Soit le passer en **public** (rien à faire sur la VM), soit
le laisser privé et faire le `docker login ghcr.io` de la VM décrit plus haut.
Sans l'un des deux, le job `deploy` échoue sur `docker compose pull`.

**7. Protéger `main` (recommandé).** Settings → Rules → Rulesets : exiger une
PR et le passage du job `build` avant merge. Un push direct sur `main` déploie
en production sans filet — c'est le comportement voulu, autant décider qui a le
droit de le déclencher.

### Premier déploiement

Étapes 1 à 5 faites, VM prête, alors :

```bash
git push origin main
```

Onglet **Actions** → run *Déploiement VM OVH*. Le job `build` doit passer
(~5-10 min la première fois, le cache `type=gha` raccourcit les suivantes) ;
`deploy` échouera tant que l'étape 6 n'est pas faite. Faire l'étape 6, puis
relancer par *Re-run failed jobs* — pas besoin de recommit.

Ensuite, chaque push sur `main` touchant le serveur redéploie tout seul.

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

## Rate limiting — en place (2026-08-05)

Token bucket nginx par IP visiteur, dans le volume NPM (survit aux
redémarrages, PAS versionné — le recréer si le volume saute) :

- `/data/nginx/custom/http_top.conf` : `limit_req_zone` `api_perip`,
  10 Mo (~160 k IP), **20 r/s** par IP, `limit_req_status 429`.
- `/data/nginx/custom/server_proxy.conf` : `limit_req zone=api_perip
  burst=40 nodelay` (+ le `real_ip_header CF-Connecting-IP` du § 3 —
  la limite compte bien l'IP du visiteur, pas celle de Cloudflare).

20 r/s est volontairement généreux : app mobile = opérateurs en CGNAT,
plusieurs utilisateurs légitimes derrière une même IP. Le burst absorbe
l'ouverture de carte (plusieurs fetch simultanés). Vérifié : ~60 requêtes
passent en rafale depuis une IP, le reste tombe en 429.

Étages complémentaires :
- **Cloudflare edge (recommandé, à faire au dashboard)** : Security → WAF →
  Rate limiting rules (1 règle incluse au plan gratuit) — ex. seuil
  300 requêtes / 10 s par IP → Block. Absorbe un flood volumétrique avant
  même la VM.
- **Quotas par uid sur les contributions** (middleware axum) : backlog.

## Rappels avant mise en production

- Retirer `/debug/ray` (tâche Notion `[MEP]`).
- L'admin NPM (port 81) ne doit jamais rester ouvert au monde.

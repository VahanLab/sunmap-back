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

## 4. Import des données OSM

L'image contient aussi le binaire `import`. Depuis la VM :

```bash
# Extrait Geofabrik → GeoJSONL (osmium requis sur la VM, hors Docker) :
scripts/osm-extract.sh europe/france/ile-de-france

# Import vers la base managée (DATABASE_URL lu depuis .env) :
docker compose run --rm api import extrait.geojsonl
```

À l'échelle Europe + Amérique, faire tourner l'extraction osmium sur une
machine costaude (ou la VM si elle a le disque) et laisser l'import tourner —
il est reprenable, l'upsert par `osm_id` rend le rejeu inoffensif.

## Rappels avant mise en production

- Retirer `/debug/ray` (tâche Notion `[MEP]`).
- Rate limiting (cf. discussion : token bucket par IP via le proxy ou en
  middleware axum, quotas par uid sur les contributions).
- L'admin NPM (port 81) ne doit jamais rester ouvert au monde.

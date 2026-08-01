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

## Rappels avant mise en production

- Retirer `/debug/ray` (tâche Notion `[MEP]`).
- Rate limiting (cf. discussion : token bucket par IP via le proxy ou en
  middleware axum, quotas par uid sur les contributions).
- L'admin NPM (port 81) ne doit jamais rester ouvert au monde.

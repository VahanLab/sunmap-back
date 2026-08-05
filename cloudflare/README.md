# Worker Cloudflare — les tuiles de `sunmap.pmtiles` en `/{z}/{x}/{y}.mvt`

Sert l'archive vectorielle stockée sur R2 comme un tileset classique :

```
https://tiles.sunmap.tech/sunmap/14/8412/5844.mvt
https://tiles.sunmap.tech/sunmap-veg/12/2074/1409.mvt
https://tiles.sunmap.tech/sunmap.json          # TileJSON
```

`sunmap` est le nom de l'archive (`sunmap.pmtiles` dans le bucket) ; le
Worker traduit `/{name}/{z}/{x}/{y}.mvt` en une lecture Range sur
`{name}.pmtiles`, avec cache au bord.

**Deux archives dans le même bucket** : `sunmap.pmtiles` (tout, z14) et
`sunmap-veg.pmtiles` (aperçu de canopée, couche `woods` seule, z12/z13, que
le client lit sous z14 — cf. `docs/tuiles-pmtiles.md`). Le `{name}` de l'URL
suffit à les distinguer : ajouter la seconde n'a demandé **aucun changement
au Worker**, seulement un second objet dans le bucket. Les deux se poussent
ensemble (`scripts/import-zone.sh --upload`) — pousser l'une sans l'autre
laisserait le client lire une canopée d'une autre époque sous z14.

## Pourquoi ce Worker plutôt qu'un lecteur PMTiles côté client

Un template `{z}/{x}/{y}` se donne **directement** à un `VectorSource`
Mapbox — c'est déjà ce que fait le relief (`ShadowMapView`, source
`mapterhorn-dem`). Le SDK iOS ne sait pas lire une archive PMTiles sans
gestionnaire de protocole custom. Le Worker évite donc à la fois d'écrire un
lecteur PMTiles en Swift (courbe de Hilbert, annuaires, gunzip) et de faire
transiter les tuiles par la VM.

Le bucket peut rester **privé** : le Worker y accède par un binding interne
(`env.BUCKET`), jamais en HTTP.

Limite à connaître : plan Workers Free = 100 000 requêtes/jour. Le cache au
bord absorbe l'essentiel (une tuile populaire n'atteint le Worker qu'une
fois par période de cache), mais c'est le plafond à surveiller si l'usage
décolle — le passage au plan payant (5 $/mois, 10 M de requêtes) est le
recours.

## Origine du code

`src/index.ts` et `src/paths.ts` viennent de
[protomaps/PMTiles](https://github.com/protomaps/PMTiles)
(`serverless/cloudflare`), BSD-3 — cf. `LICENSE.protomaps`. Seul l'import
relatif a été réécrit (`../../shared/index` → `./paths`) pour que le dossier
soit autonome. Les versions de `@cloudflare/workers-types` et `typescript`
sont montées d'un cran par rapport à l'amont, qui épinglait des versions
incompatibles avec wrangler 4.118.

Le vendorer plutôt que cloner l'amont à chaque déploiement : le
déploiement part de ce dépôt, sans dépendance à un clone externe, et une
mise à jour d'amont devient un diff qu'on relit.

## Déployer

**Node 22 minimum** (wrangler le refuse en deçà) :

```bash
nvm use 22
cd cloudflare
npm install
npx wrangler login      # OAuth navigateur, une seule fois
npm run deploy
```

`wrangler login` ouvre le navigateur et demande l'accès au compte
Cloudflare. En CI, remplacer par un jeton API (`CLOUDFLARE_API_TOKEN`) avec
la permission *Workers Scripts: Edit* — les jetons R2 (S3) ne conviennent
pas, ils ne donnent aucun droit sur les Workers.

Vérifier sans déployer : `npm run build` (compile et affiche les bindings).

## Le domaine (fait le 2026-08-03)

`tiles.sunmap.tech` a d'abord été un **domaine custom du bucket R2**.
Cloudflare refusant que le bucket et le Worker revendiquent le même nom
d'hôte, il a fallu le retirer du bucket (Dashboard → R2 → `sunmap-tiles` →
Settings → Custom Domains) avant de le déclarer dans le bloc `[[routes]]` de
`wrangler.toml`. À refaire à l'identique si le domaine change.

Vérification :

```bash
curl -I https://tiles.sunmap.tech/sunmap/14/8412/5844.mvt
```

Attendu : `200`, `content-type: application/x-protobuf`, et un en-tête
`cf-cache-status` (`MISS` au premier appel, `HIT` ensuite).

Sans bloc `[[routes]]`, le Worker reste déployé et joignable sur son URL
`*.workers.dev` — pratique pour tester avant de toucher au domaine.

## Après un réimport — ⚠ le cache ne se périme pas tout seul

Le cache du Worker est indexé sur l'URL de la tuile et **ignore le
remplacement de l'archive** : une tuile déjà servie continue de l'être dans
sa version précédente jusqu'à expiration du `CACHE_CONTROL` (1 jour par
défaut). Après un `scripts/import-zone.sh … --upload`, il faut donc purger :
dashboard → **Caching → Configuration → Purge Everything**.

C'est ce que fait `scripts/cf-purge.py`, enchaîné automatiquement par
`import-zone.sh --upload`. Il lui faut un jeton API **dédié**
(`CLOUDFLARE_PURGE_TOKEN`, permission *Zone → Cache Purge*) : le jeton OAuth
de wrangler ne porte que `zone (read)`, et celui de R2 aucun droit sur le
cache. Voir `docs/import-zone.md` § Cloudflare R2.

Depuis que le client tape le CDN en direct, versionner le nom de l'archive
(`sunmap-20260803.pmtiles`) ne dispenserait plus de la purge : il faudrait
aussi publier une version de l'app pour changer l'URL. La purge scriptée est
donc la bonne réponse ; le versionnage ne redeviendrait intéressant que si
l'URL du tileset était servie au client par configuration.

## Côté client

`VegetationTileRepository` (repo iOS) tape **ce Worker en direct**, via
`TilesConfig.baseURL` — `helios-server` n'a plus d'endpoint de tuiles, il
lit sa propre copie locale de l'archive pour ses calculs. Changer d'URL
demande donc une version de l'app : c'est le prix du chemin direct, assumé
puisque le domaine est stable.

Deux détails de protocole qui comptent :

- le Worker renvoie du MVT **déjà décompressé** (`application/x-protobuf`,
  sans `Content-Encoding`) — `MVTDecoder` reçoit du MVT en clair ;
- une tuile absente de l'archive donne **204**, pas 404 (le 404 est réservé
  à un zoom hors plage ou une archive introuvable). Le client traite les
  deux comme « tuile vide » et les met en cache, sans quoi chaque
  déplacement de carte redemanderait les mêmes tuiles vides.

Le jour où l'app consommera les tuiles avec Mapbox (`VectorSource`,
extrusions et `ModelLayer`), c'est cette même URL qu'on donnera au SDK.

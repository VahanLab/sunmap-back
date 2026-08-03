# Worker Cloudflare — les tuiles de `sunmap.pmtiles` en `/{z}/{x}/{y}.mvt`

Sert l'archive vectorielle stockée sur R2 comme un tileset classique :

```
https://tiles.sunmap.tech/sunmap/14/8412/5844.mvt
https://tiles.sunmap.tech/sunmap.json          # TileJSON
```

`sunmap` est le nom de l'archive (`sunmap.pmtiles` dans le bucket) ; le
Worker traduit `/{name}/{z}/{x}/{y}.mvt` en une lecture Range sur
`{name}.pmtiles`, avec cache au bord.

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

(Le jeton OAuth de wrangler ne porte que `zone (read)` : la purge ne peut pas
être scriptée avec lui. Il faudrait un jeton API dédié avec la permission
*Cache Purge*.)

**La parade durable serait de versionner le nom de l'archive** — téléverser
`sunmap-20260803.pmtiles` plutôt que d'écraser `sunmap.pmtiles`, puis pointer
`TILES_URL` dessus. Les URLs changent, donc aucune tuile périmée, la bascule
est atomique et le retour arrière consiste à remettre l'ancienne valeur. Pas
encore fait : à décider quand les réimports deviendront réguliers.

## Côté client — rien à changer dans l'app

`VegetationTileRepository` (repo iOS) tape toujours
`GET /vtiles/{z}/{x}/{y}` sur le serveur helios, qui **redirige (308)** vers
`$TILES_URL/sunmap/{z}/{x}/{y}.mvt` dès que `TILES_URL` est défini dans
`helios-server/.env`. URLSession suit la redirection sans rien demander : le
trafic passe par le CDN sans publier de version de l'app, et changer de
source (ou revenir en arrière) est un réglage serveur.

Le Worker renvoie du MVT **déjà décompressé** (`application/x-protobuf`,
sans `Content-Encoding`), là où l'archive locale du serveur sert les octets
gzip tels quels — dans les deux cas `MVTDecoder` reçoit du MVT en clair.

Le jour où l'app consommera les tuiles avec Mapbox (`VectorSource`,
extrusions et `ModelLayer`), c'est l'URL du Worker qu'il faudra donner
directement au SDK — pas la redirection.

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

## Libérer le domaine avant la première mise en ligne

`tiles.sunmap.tech` est aujourd'hui un **domaine custom du bucket R2**.
Cloudflare refuse que le bucket et le Worker revendiquent le même nom
d'hôte : il faut le retirer du bucket avant de le donner au Worker.

1. Dashboard → **R2** → bucket `sunmap-tiles` → **Settings** → **Custom
   Domains** → retirer `tiles.sunmap.tech`.
2. Décommenter le bloc `[[routes]]` de `wrangler.toml`.
3. `npm run deploy` — wrangler crée la route et le certificat.
4. Vérifier :
   ```bash
   curl -I https://tiles.sunmap.tech/sunmap/14/8412/5844.mvt
   ```
   Attendu : `200`, `content-type: application/x-protobuf`, et un en-tête
   `cf-cache-status` (`MISS` au premier appel, `HIT` ensuite).

Sans le bloc `[[routes]]`, le Worker est quand même déployé et joignable sur
son URL `*.workers.dev` — pratique pour tester avant de toucher au domaine.

## Après un réimport

Le cache du Worker est indexé sur l'URL et **ne se périme pas** quand
l'archive est remplacée : une tuile déjà servie reste servie dans sa version
précédente jusqu'à expiration du `CACHE_CONTROL` (1 jour par défaut). Après
un `scripts/import-zone.sh … --upload`, soit on attend, soit on purge le
cache de la zone (dashboard → Caching → Configuration → Purge Everything).

## Côté client

`VegetationTileRepository` (repo iOS) lit encore
`GET /vtiles/{z}/{x}/{y}` du serveur helios. Une fois le Worker en ligne,
l'URL devient `https://tiles.sunmap.tech/sunmap/{z}/{x}/{y}.mvt` — le
décodage ne change pas (mêmes octets MVT), et le `Content-Encoding: gzip`
du Worker est géré de façon transparente par URLSession, comme aujourd'hui.

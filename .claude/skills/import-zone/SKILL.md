---
name: import-zone
description: Importe une nouvelle zone OSM dans SunMap depuis un extrait PBF Geofabrik (établissements, mobilier urbain, bâtiments, végétation), fusionne la géométrie dans l'archive vectorielle sunmap.pmtiles, la pousse sur Cloudflare R2 et purge le cache. À utiliser quand l'utilisateur demande d'ajouter/importer/rafraîchir une zone, une ville, une région, ou de régénérer les tuiles.
---

# Importer une nouvelle zone

Procédure de référence : `docs/import-zone.md` (la lire avant d'agir).

## Commande unique

```bash
scripts/import-zone.sh <URL Geofabrik | zone.osm.pbf> [--upload] [--replace]
```

- URL des extraits : https://download.geofabrik.de/
- `--upload` : pousse `tiles/sunmap.pmtiles` sur R2 **puis purge le cache
  Cloudflare** — demander confirmation à l'utilisateur avant, c'est un envoi
  vers un service externe.
- `--replace` : repart d'une archive vide au lieu de fusionner. Destructif
  pour les zones déjà couvertes — demander confirmation.

## Points de vigilance

- **Les zones s'accumulent** : sans `--replace`, l'extrait est fusionné dans
  l'archive existante. Ajouter une région n'efface pas les précédentes. À
  identifiant OSM égal, le nouvel extrait gagne (les corrections d'OSM
  prennent effet).
- **Après un ajout, vérifier qu'une tuile de l'ANCIENNE zone répond
  encore** — c'est là qu'une régression de fusion se voit, pas dans les
  compteurs. Exemples de tuiles témoins : Paris `14/8298/5636`,
  Lyon `14/8412/5844`.
- **La purge du cache n'est pas optionnelle** après un upload : le cache du
  Worker est indexé sur l'URL et ignore le remplacement de l'archive. Sans
  elle, les tuiles restent périmées jusqu'à un jour. Elle demande
  `CLOUDFLARE_ZONE_ID` et `CLOUDFLARE_PURGE_TOKEN` (jeton dédié
  *Zone → Cache Purge*) ; absentes, `scripts/cf-purge.py` le signale sans
  faire échouer l'import.
- La géométrie ne passe **pas** par PostgreSQL : `bin/tilegen` va de
  l'extrait à l'archive. Seuls les lieux (établissements, mobilier) vont en
  base, en upsert.
- Les règles tags → hauteur vivent dans `helios-server/src/osm.rs` et
  `pbf.rs` — ne jamais les dupliquer dans un script.
- Le serveur exige `VECTOR_TILES=tiles/sunmap.pmtiles` (il refuse de
  démarrer sans). Il ne sert aucune tuile : le client tape le CDN en direct,
  d'où l'importance de la purge après upload.

## Si l'encodeur MVT est modifié

`cargo test -p helios-server vtiles` doit rester vert, en particulier
`closepath_command_is_spec_compliant`. Nos décodeurs (Rust, Swift) sont
tolérants là où **Mapbox est strict** : un `ClosePath` encodé avec un
compteur de 0 au lieu de 1 passait tous les aller-retours maison tout en
rendant les tuiles illisibles par le SDK. En cas de doute sur une commande
de géométrie, se relire contre la spec MVT 2.1, jamais contre nos propres
décodeurs. Toute évolution du schéma des couches touche `vtiles.rs`
(encodeur ET décodeur), `MVTDecoder.swift` côté iOS, et la fixture
`helios-server/testdata/mini.pmtiles`.

## Vérification après import

```bash
psql sunmap -c "SELECT count(*) FROM places;"
```

`tilegen` affiche ses comptes et, en fusion, le nombre de tuiles ayant reçu
des objets de l'archive de base.

---
name: import-zone
description: Importe une nouvelle zone OSM dans SunMap depuis un extrait PBF Geofabrik (établissements, mobilier urbain, bâtiments, végétation) puis régénère les tuiles PMTiles bâtiments + canopée. À utiliser quand l'utilisateur demande d'ajouter/importer/rafraîchir une zone, une ville, une région, ou de régénérer les tuiles.
---

# Importer une nouvelle zone

Procédure de référence : `docs/import-zone.md` (la lire avant d'agir).

## Commande unique

```bash
scripts/import-zone.sh <URL Geofabrik | zone.osm.pbf> [--upload] [--purge]
```

- URL des extraits : https://download.geofabrik.de/ (prendre le
  `-latest.osm.pbf` de la plus petite région couvrant la zone demandée).
- `--upload` : pousse `tiles/sunmap.pmtiles` sur Cloudflare R2 — demander
  confirmation à l'utilisateur avant, c'est un envoi vers un service externe.
- `--purge` : vide les tables `buildings`/`trees`/`woods` après génération —
  UNIQUEMENT si le serveur tourne avec `VECTOR_TILES=tiles/sunmap.pmtiles`,
  et demander confirmation à l'utilisateur (destructif).

## Points de vigilance

- L'import PostGIS est un upsert : relançable, réimporter rafraîchit.
- Les règles tags → hauteur vivent dans `helios-server/src/osm.rs` — ne
  jamais les dupliquer dans un script.
- L'archive `sunmap.pmtiles` (MVT z14, couches buildings/woods/trees) couvre
  l'emprise TOTALE de la base (`ST_Extent`), pas seulement la zone importée.
- Toute évolution du schéma des couches MVT = `scripts/build-pmtiles.py` ET
  `helios-server/src/vtiles.rs` ET la fixture
  (`build-pmtiles.py --fixture helios-server/testdata/mini.pmtiles`).
- `DATABASE_URL` : défaut `postgres://localhost/sunmap` ; vérifier quelle
  base est visée avant d'importer (locale vs OVH).

## Vérification après import

```bash
psql sunmap -c "SELECT (SELECT count(*) FROM buildings) AS buildings, (SELECT count(*) FROM trees) AS trees, (SELECT count(*) FROM woods) AS woods, (SELECT count(*) FROM places) AS places;"
```

Les compteurs doivent croître ; tuile canopée de l'archive = celle de
`GET /canopy/{z}/{x}/{y}` au pixel près (cf. `build-pmtiles.py --selftest`
pour la partie sans base).

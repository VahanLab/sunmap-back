---
name: import-zone
description: Importe une nouvelle zone OSM dans SunMap depuis un extrait PBF Geofabrik (établissements, mobilier urbain, bâtiments, végétation) puis régénère les tuiles PMTiles bâtiments + canopée. À utiliser quand l'utilisateur demande d'ajouter/importer/rafraîchir une zone, une ville, une région, ou de régénérer les tuiles.
---

# Importer une nouvelle zone

Procédure de référence : `docs/import-zone.md` (la lire avant d'agir).

## Commande unique

```bash
scripts/import-zone.sh <URL Geofabrik | zone.osm.pbf> [--upload]
```

- URL des extraits : https://download.geofabrik.de/ — ATTENTION : l'archive
  générée ne couvre QUE cet extrait (plus de base cumulative). Pour couvrir
  plusieurs zones, prendre un extrait englobant (ex. france-latest).
- `--upload` : pousse `tiles/sunmap.pmtiles` sur Cloudflare R2 — demander
  confirmation à l'utilisateur avant, c'est un envoi vers un service externe.

## Points de vigilance

- La géométrie ne passe PAS par PostgreSQL : `bin/tilegen` va de l'extrait à
  l'archive. Seuls les lieux (établissements, mobilier) vont en base, en
  upsert (relançable).
- Les règles tags → hauteur vivent dans `helios-server/src/osm.rs` et
  `pbf.rs` — ne jamais les dupliquer dans un script.
- Le serveur exige `VECTOR_TILES=tiles/sunmap.pmtiles` (il refuse de
  démarrer sans).
- Toute évolution du schéma des couches MVT = `vtiles.rs` (encode + decode),
  `MVTDecoder.swift` côté iOS, et la fixture
  `helios-server/testdata/mini.pmtiles`.
- `DATABASE_URL` : défaut `postgres://localhost/sunmap` ; vérifier quelle
  base est visée avant d'importer (locale vs OVH).

## Vérification après import

```bash
psql sunmap -c "SELECT count(*) FROM places;"
```

Le compteur de lieux doit croître ; `tilegen` affiche ses comptes (bâtiments,
bois, arbres) ; `cargo test vtiles` couvre l'aller-retour encodeur/lecteur.

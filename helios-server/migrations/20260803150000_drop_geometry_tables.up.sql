-- La géométrie ne vit plus en base : `sunmap.pmtiles` (archive vectorielle,
-- cf. docs/tuiles-pmtiles.md) est l'unique source des bâtiments et de la
-- végétation, générée par `bin/tilegen` directement depuis l'extrait OSM.
-- PostgreSQL ne garde que le métier : lieux, comptes, contributions.
DROP TABLE IF EXISTS buildings;
DROP TABLE IF EXISTS trees;
DROP TABLE IF EXISTS woods;
-- Le journal d'ingestion ne traçait que ces couches (l'ingestion Overpass
-- disparaît avec elles).
DROP TABLE IF EXISTS ingest_log;

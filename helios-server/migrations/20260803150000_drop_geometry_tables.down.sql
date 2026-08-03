-- Recrée les tables géométriques vides (contenu à réimporter depuis un
-- extrait PBF — définitions de 20260801083228_initial_schema).
CREATE TABLE IF NOT EXISTS buildings (
    osm_id           text PRIMARY KEY,
    name             text,
    height_m         real NOT NULL,
    height_from_osm  boolean NOT NULL,
    geom             geometry(MultiPolygon, 4326) NOT NULL
);
CREATE TABLE IF NOT EXISTS trees (
    osm_id           text PRIMARY KEY,
    height_m         double precision NOT NULL,
    crown_radius_m   double precision NOT NULL,
    geom             geometry(Point, 4326) NOT NULL,
    leaf_type        text
);
CREATE TABLE IF NOT EXISTS woods (
    osm_id           text PRIMARY KEY,
    name             text,
    height_m         real NOT NULL,
    height_from_osm  boolean NOT NULL,
    geom             geometry(MultiPolygon, 4326) NOT NULL,
    leaf_type        text
);
CREATE TABLE IF NOT EXISTS ingest_log (
    layer         text NOT NULL,
    chunk_key     text NOT NULL,
    ingested_at   timestamptz NOT NULL DEFAULT now(),
    feature_count integer NOT NULL,
    PRIMARY KEY (layer, chunk_key)
);
CREATE INDEX IF NOT EXISTS buildings_geom_idx ON buildings USING GIST (geom);
CREATE INDEX IF NOT EXISTS trees_geom_idx     ON trees     USING GIST (geom);
CREATE INDEX IF NOT EXISTS woods_geom_idx     ON woods     USING GIST (geom);

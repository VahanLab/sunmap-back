-- Schéma PostGIS de SunMap : la géométrie source du moteur d'ombre.
--
-- Ces trois tables remplacent les appels Overpass au runtime. Overpass reste
-- utilisé, mais uniquement par le binaire `ingest`, hors du chemin de requête :
-- l'API publique est lente (5-20 s par bbox), instable (504 fréquents) et
-- impose une politesse incompatible avec une requête par déplacement de carte.
--
-- Toutes les géométries sont en EPSG:4326 (lat/lon WGS84), comme les
-- coordonnées manipulées par le moteur. La reprojection en mètres n'est pas
-- nécessaire : le ray marching travaille en pixels Web Mercator, pas en SRID
-- métrique.
--
--   psql -d sunmap -f schema.sql

CREATE EXTENSION IF NOT EXISTS postgis;

-- ---------------------------------------------------------------- bâtiments

-- Un enregistrement par objet OSM porteur d'un volume : `way[building]`,
-- `way[building:part]` et `relation[building]`. Les relations arrivent en
-- MultiPolygon avec leurs anneaux intérieurs — c'est ce qui garde les cours
-- creuses au moment de la rasterisation.
CREATE TABLE IF NOT EXISTS buildings (
    osm_id           text PRIMARY KEY,          -- "way/123", "relation/456"
    name             text,
    height_m         real NOT NULL,
    -- false = hauteur déduite (médiane locale) faute de tag OSM. Conservé pour
    -- pouvoir signaler dans l'app qu'une ombre repose sur une estimation.
    height_from_osm  boolean NOT NULL,
    geom             geometry(MultiPolygon, 4326) NOT NULL
);

-- ------------------------------------------------------------------ arbres

CREATE TABLE IF NOT EXISTS trees (
    osm_id           text PRIMARY KEY,
    height_m         double precision NOT NULL,
    crown_radius_m   double precision NOT NULL,
    geom             geometry(Point, 4326) NOT NULL
);

-- --------------------------------------------------------------- terrasses

-- POI `amenity=bar|restaurant|cafe` + `outdoor_seating=yes`. La position est
-- celle d'OSM, non corrigée : le déport côté rue est calculé au runtime, il
-- dépend de la DSM et n'a rien à faire en base.
CREATE TABLE IF NOT EXISTS terraces (
    osm_id           text PRIMARY KEY,
    name             text,
    amenity          text,
    website          text,
    phone            text,
    opening_hours    text,
    cuisine          text,
    wikidata         text,
    geom             geometry(Point, 4326) NOT NULL
);

-- ----------------------------------------------------------------- index

-- GIST sur chaque géométrie : c'est ce qui rend le `&&` par bounding box
-- utilisable à chaque déplacement de carte.
CREATE INDEX IF NOT EXISTS buildings_geom_idx ON buildings USING GIST (geom);
CREATE INDEX IF NOT EXISTS trees_geom_idx     ON trees     USING GIST (geom);
CREATE INDEX IF NOT EXISTS terraces_geom_idx  ON terraces  USING GIST (geom);

-- ------------------------------------------------------ suivi d'ingestion

-- L'init de Paris passe par plusieurs dizaines de requêtes Overpass. Tracer
-- les tuiles déjà absorbées rend l'ingestion reprenable après un 504 ou une
-- coupure, au lieu de tout refaire.
CREATE TABLE IF NOT EXISTS ingest_log (
    layer        text NOT NULL,   -- 'buildings' | 'trees' | 'terraces'
    chunk_key    text NOT NULL,   -- bbox arrondie de la tuile Overpass
    ingested_at  timestamptz NOT NULL DEFAULT now(),
    feature_count integer NOT NULL,
    PRIMARY KEY (layer, chunk_key)
);

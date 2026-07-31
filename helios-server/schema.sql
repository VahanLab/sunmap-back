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

-- ------------------------------------------------------------------ bois

-- Emprises boisées : forêts, bois, alignements d'arbres.
--
-- Table à part des bâtiments malgré une forme identique — un contour et une
-- hauteur — parce que leur ombre n'est pas de même nature : un bâtiment est
-- opaque, une canopée laisse passer une partie du soleil. La distinction est
-- portée par la table plutôt que par une colonne, pour que rien ne puisse
-- confondre les deux en chemin.
--
-- OSM ne donne jamais la hauteur d'une forêt ; celle stockée ici est un repli
-- par type, à remplacer par un modèle de hauteur de canopée (Meta/WRI CHM ou
-- IGN MNH).
CREATE TABLE IF NOT EXISTS woods (
    osm_id           text PRIMARY KEY,
    name             text,
    height_m         real NOT NULL,
    height_from_osm  boolean NOT NULL,
    geom             geometry(MultiPolygon, 4326) NOT NULL
);

-- --------------------------------------------------------------- terrasses

-- Établissements de restauration et de boisson (cf. `osm::AMENITIES`).
--
-- Aucun filtre sur `outdoor_seating` à l'ingestion : le tag est très
-- inégalement renseigné dans OSM, et filtrer dessus faisait disparaître
-- beaucoup d'établissements qui ont bel et bien une terrasse. On stocke le tag
-- tel quel et on laisse le filtre au client.
--
-- La position est celle d'OSM, non corrigée : le déport côté rue est calculé au
-- runtime, il dépend de la DSM et n'a rien à faire en base.
CREATE TABLE IF NOT EXISTS places (
    osm_id           text PRIMARY KEY,
    name             text,
    amenity          text,
    -- Trois états : true = terrasse, false = refus explicite (`no`),
    -- NULL = non renseigné. Ce dernier cas couvre ~79 % des établissements
    -- parisiens et ne veut PAS dire qu'il n'y a pas de terrasse — le client
    -- doit donc s'abstenir plutôt que d'afficher « pas de terrasse ».
    outdoor_seating  boolean,
    website          text,
    phone            text,
    opening_hours    text,
    cuisine          text,
    wikidata         text,
    geom             geometry(Point, 4326) NOT NULL
);

-- ------------------------------------------------- contributions utilisateur

-- --------------------------------------------------------------- comptes

-- Identité vérifiée par Firebase, pseudo tenu ici.
--
-- Le pseudo vit en PostgreSQL et non côté Firebase : c'est une donnée métier,
-- affichée à côté des contributions, et la contrainte d'unicité d'une base est
-- la seule garantie qui tienne sous concurrence — deux inscriptions simultanées
-- sur le même pseudo se départagent ici, pas dans le client.
CREATE TABLE IF NOT EXISTS users (
    -- `sub` du jeton Firebase. Stable pour un compte, quel que soit le
    -- fournisseur employé pour s'y connecter (mot de passe, Google, Apple).
    uid          text PRIMARY KEY,
    -- Casse d'origine, pour l'affichage.
    username     text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    -- Unicité insensible à la casse : « Karl » et « karl » désignent la même
    -- personne aux yeux d'un lecteur, les laisser coexister inviterait à
    -- l'usurpation. Chacun garde malgré tout la casse qu'il a choisie.
    username_key text GENERATED ALWAYS AS (lower(username)) STORED UNIQUE,
    -- Le serveur valide déjà la forme pour renvoyer un message clair ; la base
    -- reste le garde-fou, y compris pour un import ou une correction à la main.
    CONSTRAINT username_shape CHECK (username ~ '^[A-Za-z0-9_.]{3,20}$')
);

-- Terrasse signalée par un utilisateur : sa présence, et surtout sa position
-- exacte, qu'OSM ne donne jamais (le nœud d'un bar est posé sur le bâtiment).
--
-- Table SÉPARÉE de `places`, et c'est essentiel : `bin/import` fait un upsert
-- sur `places` à chaque réimport d'extrait OSM, ce qui effacerait toute colonne
-- de contribution qu'on y aurait ajoutée. Ici les deux cycles de vie
-- n'interfèrent pas.
--
-- Pas de clé étrangère vers `places` : un établissement peut disparaître d'OSM
-- puis revenir, et on ne veut pas perdre la contribution entre-temps.
CREATE TABLE IF NOT EXISTS place_terraces (
    osm_id       text PRIMARY KEY,
    -- L'utilisateur affirme la présence ou l'absence de terrasse. Prime sur le
    -- tag OSM, y compris pour le contredire.
    has_terrace  boolean NOT NULL,
    -- Position de la terrasse. NULL quand `has_terrace` est faux, ou quand
    -- l'utilisateur signale une terrasse sans la situer.
    geom         geometry(Point, 4326),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    -- Auteur de la contribution. NULL pour celles d'avant l'authentification.
    -- `ON DELETE SET NULL` : supprimer un compte ne doit pas emporter les
    -- terrasses qu'il a signalées, elles restent vraies sans lui.
    user_uid     text REFERENCES users(uid) ON DELETE SET NULL
);

-- ------------------------------------------------------------ migrations

-- Mobilier urbain (bancs `amenity=bench`, tables `leisure=picnic_table`),
-- logé dans `places` : même pipeline bbox + classification soleil/ombre que
-- les établissements. Colonnes NULL pour tout le reste de la table.
ALTER TABLE places
    ADD COLUMN IF NOT EXISTS direction_deg real,
    ADD COLUMN IF NOT EXISTS covered boolean,
    ADD COLUMN IF NOT EXISTS backrest boolean,
    ADD COLUMN IF NOT EXISTS seats integer,
    ADD COLUMN IF NOT EXISTS material text;

-- `CREATE TABLE IF NOT EXISTS` ne modifie pas une table déjà présente : les
-- bases créées avant l'authentification n'auraient jamais la colonne d'auteur.
ALTER TABLE place_terraces
    ADD COLUMN IF NOT EXISTS user_uid text REFERENCES users(uid) ON DELETE SET NULL;

-- Historique des signalements de terrasse, à côté de `place_terraces` qui ne
-- garde que le dernier. Même esprit que `place_furniture_contributions` :
-- savoir qui a signalé en premier, et si quelqu'un d'autre a corrigé depuis,
-- demande de ne rien écraser.
CREATE TABLE IF NOT EXISTS place_terrace_contributions (
    id           bigserial PRIMARY KEY,
    place_id     text NOT NULL REFERENCES places(osm_id) ON DELETE CASCADE,
    user_uid     text REFERENCES users(uid) ON DELETE SET NULL,
    has_terrace  boolean NOT NULL,
    lat          double precision,
    lng          double precision,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS place_terrace_contributions_place_idx
    ON place_terrace_contributions (place_id, created_at DESC);

-- Banc ou table de pique-nique ajouté depuis l'app plutôt qu'importé d'OSM.
-- NULL pour toutes les lignes venues de `bin/import` ou `bin/ingest` : ceux-là
-- ne référencent jamais un compte. Contrairement aux terrasses, pas de table
-- séparée — l'`osm_id` synthétique (`user/<uuid>`) ne collisionne jamais avec
-- un identifiant OSM réel, donc rien à protéger d'un réimport.
--
-- Auteur **courant** de ce qui est affiché : celui qui a posé le meuble, ou le
-- dernier à en avoir corrigé position/orientation/dossier. NULL pour un meuble
-- OSM jamais retouché — quiconque peut alors le prendre en charge le premier.
ALTER TABLE places
    ADD COLUMN IF NOT EXISTS contributor_uid text REFERENCES users(uid) ON DELETE SET NULL;

-- Historique de toutes les contributions sur un même meuble, et pas seulement
-- la dernière : deux personnes peuvent en avoir une lecture différente (la
-- terrasse du café d'à côté a bougé le banc), et effacer les versions
-- précédentes reviendrait à trancher entre elles sans le dire. Seul
-- `applied` distingue ce qui est effectivement affiché de ce qui reste une
-- proposition concurrente — cf. `db::submit_furniture_contribution` : une
-- contribution d'un autre auteur que le contributeur courant s'ajoute ici
-- sans toucher à `places`.
CREATE TABLE IF NOT EXISTS place_furniture_contributions (
    id           bigserial PRIMARY KEY,
    place_id     text NOT NULL REFERENCES places(osm_id) ON DELETE CASCADE,
    user_uid     text REFERENCES users(uid) ON DELETE SET NULL,
    lat          double precision NOT NULL,
    lng          double precision NOT NULL,
    direction_deg real,
    backrest     boolean,
    applied      boolean NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS place_furniture_contributions_place_idx
    ON place_furniture_contributions (place_id, created_at DESC);

-- ----------------------------------------------------------------- index

-- GIST sur chaque géométrie : c'est ce qui rend le `&&` par bounding box
-- utilisable à chaque déplacement de carte.
CREATE INDEX IF NOT EXISTS buildings_geom_idx ON buildings USING GIST (geom);
CREATE INDEX IF NOT EXISTS trees_geom_idx     ON trees     USING GIST (geom);
CREATE INDEX IF NOT EXISTS woods_geom_idx     ON woods     USING GIST (geom);
CREATE INDEX IF NOT EXISTS places_geom_idx  ON places  USING GIST (geom);
CREATE INDEX IF NOT EXISTS place_terraces_geom_idx ON place_terraces USING GIST (geom);

-- ------------------------------------------------------ suivi d'ingestion

-- L'init de Paris passe par plusieurs dizaines de requêtes Overpass. Tracer
-- les tuiles déjà absorbées rend l'ingestion reprenable après un 504 ou une
-- coupure, au lieu de tout refaire.
CREATE TABLE IF NOT EXISTS ingest_log (
    layer        text NOT NULL,   -- 'buildings' | 'trees' | 'places'
    chunk_key    text NOT NULL,   -- bbox arrondie de la tuile Overpass
    ingested_at  timestamptz NOT NULL DEFAULT now(),
    feature_count integer NOT NULL,
    PRIMARY KEY (layer, chunk_key)
);

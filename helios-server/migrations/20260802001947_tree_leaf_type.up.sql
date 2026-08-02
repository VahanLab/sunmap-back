-- Silhouette de la végétation, pour choisir le modèle 3D à l'affichage :
-- 'broadleaved' (défaut), 'needleleaved', 'palm'. Vient du tag OSM
-- `leaf_type`, à défaut déduit du genre (cf. `osm::LeafType::from_tags`).
--
-- NULL sur les lignes importées avant cette colonne : le client retombe
-- alors sur le feuillu, la silhouette de ~80 % des arbres urbains d'Europe.
ALTER TABLE trees ADD COLUMN IF NOT EXISTS leaf_type text;
ALTER TABLE woods ADD COLUMN IF NOT EXISTS leaf_type text;

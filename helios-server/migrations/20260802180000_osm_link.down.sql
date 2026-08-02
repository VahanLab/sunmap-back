DROP TABLE IF EXISTS osm_pushes;
DROP INDEX IF EXISTS users_osm_user_id_key;
ALTER TABLE users
    DROP COLUMN IF EXISTS osm_user_id,
    DROP COLUMN IF EXISTS osm_display_name,
    DROP COLUMN IF EXISTS osm_access_token,
    DROP COLUMN IF EXISTS osm_refresh_token,
    DROP COLUMN IF EXISTS osm_token_expires_at,
    DROP COLUMN IF EXISTS osm_linked_at;

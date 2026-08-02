-- Liaison d'un compte SunMap à un compte OpenStreetMap, pour que les
-- contributions faites dans l'app remontent à la source.
--
-- Le jeton vit ici et **pas sur l'appareil** : c'est le serveur qui pousse, y
-- compris en différé après un échec réseau, et un jeton d'écriture OSM baladé
-- dans une app est un jeton qu'on ne peut plus révoquer proprement.
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS osm_user_id      bigint,
    ADD COLUMN IF NOT EXISTS osm_display_name text,
    ADD COLUMN IF NOT EXISTS osm_access_token text,
    ADD COLUMN IF NOT EXISTS osm_refresh_token text,
    -- `NULL` = jeton sans expiration connue. OSM en délivre aujourd'hui de
    -- permanents, mais l'API annonce pouvoir changer d'avis : mieux vaut la
    -- colonne dès maintenant qu'une migration en catastrophe.
    ADD COLUMN IF NOT EXISTS osm_token_expires_at timestamptz,
    ADD COLUMN IF NOT EXISTS osm_linked_at    timestamptz;

-- Un compte OSM ne se lie qu'à un seul compte SunMap : deux comptes qui
-- pousseraient sous la même identité rendraient les changesets illisibles, et
-- révoquer l'un ne révoquerait pas l'autre.
CREATE UNIQUE INDEX IF NOT EXISTS users_osm_user_id_key
    ON users (osm_user_id) WHERE osm_user_id IS NOT NULL;

-- File d'attente des envois vers OSM.
--
-- Une table, et pas un envoi direct dans la requête HTTP : l'API OSM peut être
-- lente, en maintenance ou refuser un changeset, et rien de tout cela ne doit
-- faire échouer la contribution côté SunMap. La carte reste juste, l'envoi
-- se rattrape.
CREATE TABLE IF NOT EXISTS osm_pushes (
    id           bigserial PRIMARY KEY,
    user_uid     text REFERENCES users(uid) ON DELETE SET NULL,
    -- 'terrace' | 'bench' | 'picnic_table'
    kind         text NOT NULL,
    -- Établissement visé côté SunMap. Pour une terrasse c'est l'identifiant
    -- OSM réel ; pour du mobilier ajouté, l'identifiant synthétique `user/…`.
    place_id     text NOT NULL,
    -- Charge utile de l'envoi (tags, position), figée au moment de la
    -- contribution : rejouer un envoi doit rejouer ce qui a été demandé, pas
    -- l'état courant de la base.
    payload      jsonb NOT NULL,
    -- 'pending' | 'sent' | 'failed'
    status       text NOT NULL DEFAULT 'pending',
    attempts     int NOT NULL DEFAULT 0,
    -- Renseignés au succès : de quoi retrouver la modification dans OSM.
    osm_element  text,
    changeset_id bigint,
    last_error   text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS osm_pushes_pending_idx
    ON osm_pushes (status, created_at) WHERE status = 'pending';

-- Interdiction de contribuer, posée à la main sur un compte problématique
-- (troll, spam) : `UPDATE users SET banned = true WHERE username = '…'`.
-- La consultation reste libre — seul l'écrit est fermé, et le client reçoit
-- un 403 au code dédié (`contribution_banned`) pour l'expliquer.
ALTER TABLE users ADD COLUMN banned boolean NOT NULL DEFAULT false;

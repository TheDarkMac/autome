-- Tokens OAuth des connecteurs, par utilisateur (backlog post-audit —
-- credentials par propriétaire du run).
--
-- Une ligne par `(user_id, provider_id)` : le dernier `TokenSet` obtenu via
-- `/credentials/{provider}/callback`, que le worker relit pour les runs dont
-- l'utilisateur est propriétaire (`runs.owner_id`).
--
-- `envelope` : `TokenSet` sérialisé puis chiffré par `EnvelopeCipher`
-- (`v1:<key_id>:<nonce>:<ciphertext>`, AES-256-GCM) — **jamais en clair**.
-- Le `key_id` embarqué permet la rotation de clé sans migration.
--
-- `user_id` en `TEXT` sans FK vers `users` : même convention que
-- `user_permissions.user_id` et `runs.owner_id` (identité = `claims.sub`).
-- La table `credentials` (0004, UUID + FK, jamais câblée) n'est pas réutilisée.
CREATE TABLE connector_tokens (
    user_id       TEXT   NOT NULL CHECK (user_id <> ''),
    provider_id   TEXT   NOT NULL CHECK (provider_id <> ''),
    envelope      TEXT   NOT NULL CHECK (envelope <> ''),
    updated_at_ms BIGINT NOT NULL,
    -- La PK couvre aussi `list_by_user` (préfixe `user_id`, tri `provider_id`).
    PRIMARY KEY (user_id, provider_id)
);

-- Administration des utilisateurs (#208).
--
-- Suspension réversible d'un compte par un administrateur : `disabled_at`
-- non nul ⇒ connexion refusée (l'API ferme en outre toutes ses sessions en
-- incrémentant sa version de session, cf. 0013). Le compte garde ses données
-- et ses permissions ; le réactiver remet `disabled_at` à NULL.
ALTER TABLE users ADD COLUMN disabled_at TIMESTAMPTZ NULL;

-- Liste d'administration paginée dans l'ordre de création (`ORDER BY
-- created_at, id LIMIT … OFFSET …`) : parcours d'index sans tri.
CREATE INDEX users_created_at_id_idx ON users (created_at, id);

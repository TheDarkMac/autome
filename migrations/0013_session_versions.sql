-- Version de session par utilisateur : « déconnecter toutes les sessions » (#186).
--
-- Chaque JWT porte la version de son utilisateur à l'émission (claim `sv`).
-- `POST /auth/logout-all` incrémente `version` : tous les jetons émis avant
-- (access comme refresh, sur tous les appareils) sont refusés, sans avoir à
-- les énumérer dans `revoked_tokens`.
--
-- Pas de ligne ⇒ version 0 (aucune déconnexion générale). Pas de clé étrangère
-- vers `users`, comme `revoked_tokens` : la table ne référence que le `sub`
-- des jetons. Une ligne par utilisateur l'ayant utilisée, jamais purgée
-- (la supprimer revaliderait les anciens jetons).
CREATE TABLE session_versions (
    user_id TEXT   PRIMARY KEY,
    version BIGINT NOT NULL CHECK (version >= 1)
);

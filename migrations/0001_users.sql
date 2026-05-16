-- F1.5.b — Table `users`
-- Réf. modèle ER : docs/ARCHITECTURE.md §6.
-- PostgreSQL >= 13 : gen_random_uuid() est natif (aucune extension requise).

CREATE TABLE users (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    email         TEXT        NOT NULL,
    password_hash TEXT        NOT NULL,
    -- Rôle RBAC ; le modèle fin (rôles/permissions) relève de F6.5.
    role          TEXT        NOT NULL DEFAULT 'user',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT users_email_unique UNIQUE (email),
    CONSTRAINT users_role_check CHECK (role IN ('user', 'admin'))
);

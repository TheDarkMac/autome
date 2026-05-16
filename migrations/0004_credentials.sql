-- F1.5.e — Table `credentials`
-- Le secret est stocké chiffré au repos (bytea). L'algorithme de
-- chiffrement/déchiffrement transparent (AES-GCM + KMS) et la rotation de
-- clé relèvent de F6.3 — ici on pose uniquement la structure de stockage.

CREATE TABLE credentials (
    id         UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id   UUID        NOT NULL,
    -- Provider tiers (ex. 'slack', 'gmail', 'notion', 'github').
    provider   TEXT        NOT NULL,
    -- Libellé choisi par l'utilisateur (plusieurs comptes par provider).
    name       TEXT        NOT NULL,
    -- Charge utile chiffrée (jamais en clair, jamais loggée — cf. F6.3).
    secret_enc BYTEA       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT credentials_owner_fk
        FOREIGN KEY (owner_id) REFERENCES users (id) ON DELETE CASCADE,
    CONSTRAINT credentials_unique_per_owner UNIQUE (owner_id, provider, name)
);

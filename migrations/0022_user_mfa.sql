-- Second facteur TOTP (#245).
--
-- `user_mfa` : un enrôlement par compte. `secret` est le secret TOTP
-- **chiffré** (enveloppe AES-GCM, clé dérivée d'`ENCRYPTION_KEY`) : une copie
-- de la base ne suffit pas à générer des codes. `enabled_at` nul ⇒ enrôlement
-- en attente du premier code (le secret peut encore être remplacé) ; non nul
-- ⇒ MFA active, exigée à la connexion. `last_step` : dernier pas de temps
-- TOTP accepté ; un code n'est accepté que pour un pas **postérieur**
-- (écriture conditionnelle) : un code intercepté ne sert pas deux fois.
--
-- `mfa_recovery_codes` : codes de secours à usage unique, stockés sous forme
-- d'empreinte HMAC-SHA-256 (clé dérivée d'`ENCRYPTION_KEY`), jamais en clair.
-- Un code utilisé est supprimé.
--
-- Clés étrangères en cascade : effacer le compte (#229) efface son second
-- facteur.
CREATE TABLE user_mfa (
    user_id    UUID        PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    secret     TEXT        NOT NULL,
    enabled_at TIMESTAMPTZ NULL,
    last_step  BIGINT      NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE mfa_recovery_codes (
    user_id   UUID NOT NULL REFERENCES user_mfa (user_id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL,
    PRIMARY KEY (user_id, code_hash)
);

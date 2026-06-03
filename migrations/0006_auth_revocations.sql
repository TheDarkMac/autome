-- Liste de révocation des JWT (dette #107, constat 2).
-- Backe `RevocationStore` côté PostgreSQL (cf. autome-persistence) :
-- persistée et partagée entre répliques, contrairement à l'impl in-memory.
CREATE TABLE revoked_tokens (
    -- Identifiant unique du token révoqué (claim `jti`).
    jti    TEXT   PRIMARY KEY,
    -- Expiration de la révocation (ms Unix) : au-delà, le token est de toute
    -- façon expiré → l'entrée peut être collectée (`gc`).
    exp_ms BIGINT NOT NULL
);

-- Accélère le GC périodique (`DELETE ... WHERE exp_ms <= now`).
CREATE INDEX idx_revoked_tokens_exp ON revoked_tokens (exp_ms);

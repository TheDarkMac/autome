-- Endpoints des déclencheurs webhook (#202).
--
-- Chaque nœud `trigger_type: "webhook"` reçoit une URL secrète `/hooks/{token}`.
-- Le token est dérivé par l'API d'une clé tirée d'`ENCRYPTION_KEY` et du sel
-- ci-dessous : la table ne contient ni le token ni le secret de signature,
-- seulement le sel et le SHA-256 du token (clé de résolution d'une requête
-- entrante). Une rotation remplace les deux.
--
-- Les endpoints disparaissent avec leur workflow (cascade) ; ceux d'un nœud
-- retiré sont purgés par l'API à la mise à jour du workflow.

CREATE TABLE webhook_endpoints (
    owner_id    TEXT        NOT NULL,
    workflow_id TEXT        NOT NULL,
    node_id     TEXT        NOT NULL,
    salt        BYTEA       NOT NULL CHECK (octet_length(salt) = 32),
    token_hash  BYTEA       NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (owner_id, workflow_id, node_id),
    CONSTRAINT webhook_endpoints_workflow_fkey
        FOREIGN KEY (owner_id, workflow_id) REFERENCES workflows (owner_id, id)
        ON DELETE CASCADE
);

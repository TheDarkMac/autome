-- F1.5.c — Table `workflows` + versioning (`workflow_versions`)
-- Le graphe (jsonb) suit le modèle F1.4 (autome-core::WorkflowGraph).

CREATE TABLE workflows (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id        UUID        NOT NULL,
    name            TEXT        NOT NULL,
    active          BOOLEAN     NOT NULL DEFAULT FALSE,
    -- Pointe la version courante dans workflow_versions (NULL tant qu'aucune
    -- version publiée).
    current_version INTEGER,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT workflows_owner_fk
        FOREIGN KEY (owner_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE workflow_versions (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    workflow_id UUID        NOT NULL,
    version     INTEGER     NOT NULL,
    -- Sérialisation de autome-core::WorkflowGraph (F1.4).
    graph       JSONB       NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT workflow_versions_workflow_fk
        FOREIGN KEY (workflow_id) REFERENCES workflows (id) ON DELETE CASCADE,
    CONSTRAINT workflow_versions_unique UNIQUE (workflow_id, version),
    CONSTRAINT workflow_versions_version_positive CHECK (version >= 1)
);

-- `current_version` doit référencer une version existante du workflow.
-- (Contrainte composite différable posée en 0005 pour éviter l'ordre d'insert.)

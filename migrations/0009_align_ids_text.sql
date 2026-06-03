-- Réalignement du schéma sur le domaine (dette #107, step 3b).
--
-- Les types métier (`autome_core::WorkflowGraph`, `RunCheckpoint`) utilisent
-- des **identifiants logiques `String`** (ex. `"wf-pipe"`), pas des UUID, et
-- n'ont pas de notion de propriétaire. On réaligne donc le schéma F1.5 :
--   - PK / FK en **TEXT** (id logique = `WorkflowId` / `run_id`) ;
--   - suppression de `workflows.owner_id` (le graphe n'a pas d'owner) ;
--   - timestamps en **`BIGINT` ms Unix** (cohérents avec les `*_at_ms`) ;
--   - ajout de `runs.next_node` / `runs.error` et `run_steps.output` pour
--     porter fidèlement un `RunCheckpoint`.
--
-- Base gérée par le runner (pas de données en prod) : on recrée les tables.
-- `CASCADE` retire aussi les contraintes inter-tables (dont la FK différable
-- `workflows.current_version` posée en 0005) sans dépendre de l'ordre.
DROP TABLE IF EXISTS run_steps, runs, workflow_versions, workflows CASCADE;

CREATE TABLE workflows (
    id              TEXT    PRIMARY KEY,        -- = WorkflowId (id logique)
    name            TEXT    NOT NULL,
    active          BOOLEAN NOT NULL DEFAULT FALSE,
    current_version INTEGER,                    -- version courante (workflow_versions)
    created_at_ms   BIGINT  NOT NULL DEFAULT 0,
    updated_at_ms   BIGINT  NOT NULL DEFAULT 0
);

CREATE TABLE workflow_versions (
    workflow_id   TEXT    NOT NULL REFERENCES workflows (id) ON DELETE CASCADE,
    version       INTEGER NOT NULL CHECK (version >= 1),
    -- Sérialisation complète de autome_core::WorkflowGraph (F1.4).
    graph         JSONB   NOT NULL,
    created_at_ms BIGINT  NOT NULL DEFAULT 0,
    PRIMARY KEY (workflow_id, version)
);

CREATE TABLE runs (
    id             TEXT    PRIMARY KEY,         -- = run_id (id logique)
    -- Pas de FK : un run peut référencer un workflow déjà supprimé (cohérent
    -- avec l'impl in-memory qui ne contraint pas).
    workflow_id    TEXT    NOT NULL,
    version        INTEGER NOT NULL,
    status         TEXT    NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    trigger_type   TEXT    NOT NULL,
    started_at_ms  BIGINT,
    finished_at_ms BIGINT,
    created_at_ms  BIGINT  NOT NULL,
    next_node      TEXT,
    error          TEXT
);
CREATE INDEX idx_runs_workflow ON runs (workflow_id, created_at_ms DESC);
CREATE INDEX idx_runs_status ON runs (status, started_at_ms);

CREATE TABLE run_steps (
    run_id  TEXT  NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
    node_id TEXT  NOT NULL,
    -- Sortie calculée du node (RunCheckpoint.outputs[node_id]).
    output  JSONB NOT NULL,
    PRIMARY KEY (run_id, node_id)
);

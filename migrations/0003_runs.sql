-- F1.5.d — Tables `runs` & `run_steps` (état d'exécution)
-- Statuts alignés sur autome-engine::RunStatus (F1.3.c) ; la persistance de
-- reprise après crash (checkpoints) est détaillée en F5.4.

CREATE TABLE runs (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    workflow_id  UUID        NOT NULL,
    -- Version exacte exécutée (le workflow peut évoluer après le run).
    version      INTEGER     NOT NULL,
    status       TEXT        NOT NULL DEFAULT 'queued',
    trigger_type TEXT        NOT NULL,
    started_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT runs_workflow_fk
        FOREIGN KEY (workflow_id) REFERENCES workflows (id) ON DELETE CASCADE,
    CONSTRAINT runs_status_check
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed'))
);

CREATE TABLE run_steps (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id      UUID        NOT NULL,
    -- Identifiant logique du node (autome-core::NodeId, F1.4).
    node_id     TEXT        NOT NULL,
    status      TEXT        NOT NULL DEFAULT 'queued',
    input       JSONB,
    output      JSONB,
    error       TEXT,
    started_at  TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT run_steps_run_fk
        FOREIGN KEY (run_id) REFERENCES runs (id) ON DELETE CASCADE,
    CONSTRAINT run_steps_status_check
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'skipped')),
    -- Un node n'apparaît qu'une fois par run (les ré-exécutions de boucle
    -- F2.4 seront agrégées dans `output`, pas dupliquées en lignes).
    CONSTRAINT run_steps_unique UNIQUE (run_id, node_id)
);

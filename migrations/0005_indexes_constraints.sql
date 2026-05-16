-- F1.5.f — Index & contraintes complémentaires
-- Index secondaires sur les colonnes de jointure/filtre fréquentes
-- (les PK et UNIQUE sont déjà indexés par leurs contraintes).

-- Jointures par clé étrangère.
CREATE INDEX idx_workflows_owner            ON workflows (owner_id);
CREATE INDEX idx_workflow_versions_workflow ON workflow_versions (workflow_id);
CREATE INDEX idx_runs_workflow              ON runs (workflow_id);
CREATE INDEX idx_run_steps_run              ON run_steps (run_id);
CREATE INDEX idx_credentials_owner          ON credentials (owner_id);

-- Accès opérationnels : file d'exécution et historique récent.
CREATE INDEX idx_runs_status      ON runs (status);
CREATE INDEX idx_runs_created_at  ON runs (created_at DESC);

-- Intégrité : `workflows.current_version` doit pointer une version réelle
-- DU MÊME workflow. FK composite différable (l'insertion workflow →
-- version → maj current_version se fait dans une transaction).
ALTER TABLE workflows
    ADD CONSTRAINT workflows_current_version_fk
        FOREIGN KEY (id, current_version)
        REFERENCES workflow_versions (workflow_id, version)
        DEFERRABLE INITIALLY DEFERRED;

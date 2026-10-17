-- Échéances des déclencheurs planifiés (#227) : nœuds `schedule` (cron /
-- intervalle) et `polling`.
--
-- Une ligne par nœud déclencheur, tenue à jour par l'API à l'enregistrement du
-- workflow ; supprimée avec lui (cascade). Les workers lisent les échéances
-- atteintes et les avancent par compare-and-set sur `revision` : une seule
-- réplique déclenche une échéance donnée.
--
-- `next_fire_ms` NULL = échéance à calculer (nœud nouveau ou modifié) : le
-- premier worker qui la voit l'arme sans déclencher. `state` porte l'état d'un
-- polling (clés déjà vues). `error` : paramètres inutilisables, nœud ignoré
-- jusqu'à sa prochaine modification.

CREATE TABLE trigger_schedules (
    owner_id     TEXT    NOT NULL,
    workflow_id  TEXT    NOT NULL,
    node_id      TEXT    NOT NULL,
    trigger_type TEXT    NOT NULL,
    params       JSONB   NOT NULL,
    next_fire_ms BIGINT,
    state        JSONB   NOT NULL DEFAULT '{}'::jsonb,
    error        TEXT,
    revision     BIGINT  NOT NULL DEFAULT 0,
    PRIMARY KEY (owner_id, workflow_id, node_id),
    CONSTRAINT trigger_schedules_workflow_fkey
        FOREIGN KEY (owner_id, workflow_id) REFERENCES workflows (owner_id, id)
        ON DELETE CASCADE
);

-- Lecture des échéances atteintes (ou à calculer), hors nœuds en erreur.
CREATE INDEX idx_trigger_schedules_due
    ON trigger_schedules (next_fire_ms NULLS FIRST)
    WHERE error IS NULL;

-- Reprise des workflows existants : nœuds planifiés de la version courante,
-- échéance à calculer par le premier worker.
INSERT INTO trigger_schedules (owner_id, workflow_id, node_id, trigger_type, params)
SELECT w.owner_id, w.id, n->>'id', n->>'trigger_type', COALESCE(n->'params', 'null'::jsonb)
FROM workflows w
JOIN workflow_versions v
  ON v.owner_id = w.owner_id AND v.workflow_id = w.id AND v.version = w.current_version
CROSS JOIN LATERAL jsonb_array_elements(
    CASE WHEN jsonb_typeof(v.graph->'nodes') = 'array' THEN v.graph->'nodes' ELSE '[]'::jsonb END
) AS n
WHERE n->>'kind' = 'trigger'
  AND n->>'trigger_type' IN ('schedule', 'polling')
  AND n->>'id' IS NOT NULL
ON CONFLICT DO NOTHING;

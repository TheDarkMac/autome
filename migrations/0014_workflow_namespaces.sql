-- Ids de workflow namespacés par propriétaire (#188).
--
-- `workflows.id` était une clé globale, choisie par le client : créer un id
-- déjà pris par un autre utilisateur répondait 409 (sonde d'existence des
-- workflows d'autrui) et le premier créateur confisquait l'id pour tous. Le
-- frontend utilise l'id fixe `wf-draft` : seul le premier compte de l'instance
-- pouvait le créer.
--
-- L'identité d'un workflow devient `(owner_id, id)` : chaque utilisateur a son
-- espace d'ids. Les workflows sans propriétaire (antérieurs à 0010) passent
-- dans l'espace `''` (`LEGACY_NAMESPACE`), réservé aux super-admins — aucun
-- `claims.sub` n'est vide. Les runs ne sont pas touchés : ils portent déjà
-- `owner_id` (0010) et le worker relit le workflow dans l'espace du
-- propriétaire du run.

UPDATE workflows SET owner_id = '' WHERE owner_id IS NULL;
ALTER TABLE workflows ALTER COLUMN owner_id SET NOT NULL;

-- Les versions portent le propriétaire pour référencer la clé composite.
ALTER TABLE workflow_versions ADD COLUMN owner_id TEXT;
UPDATE workflow_versions v SET owner_id = w.owner_id FROM workflows w WHERE w.id = v.workflow_id;
ALTER TABLE workflow_versions ALTER COLUMN owner_id SET NOT NULL;

ALTER TABLE workflow_versions DROP CONSTRAINT workflow_versions_workflow_id_fkey;
ALTER TABLE workflow_versions DROP CONSTRAINT workflow_versions_pkey;
ALTER TABLE workflows DROP CONSTRAINT workflows_pkey;

ALTER TABLE workflows ADD CONSTRAINT workflows_pkey PRIMARY KEY (owner_id, id);
ALTER TABLE workflow_versions
    ADD CONSTRAINT workflow_versions_pkey PRIMARY KEY (owner_id, workflow_id, version);
ALTER TABLE workflow_versions
    ADD CONSTRAINT workflow_versions_workflow_fkey
        FOREIGN KEY (owner_id, workflow_id) REFERENCES workflows (owner_id, id)
        ON DELETE CASCADE;

-- Couvert par la nouvelle clé primaire (même colonnes, même ordre).
DROP INDEX idx_workflows_owner_id;

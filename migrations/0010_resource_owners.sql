-- Isolation par propriétaire (backlog post-audit — BOLA).
--
-- 0009 avait retiré `workflows.owner_id` : tout porteur de `workflows.read`
-- voyait (et avec `workflows.write`, modifiait) les workflows de tous. On
-- rattache chaque workflow et chaque run au `claims.sub` de son propriétaire.
--
-- `TEXT` (et non UUID + FK vers users) : même convention que
-- `user_permissions.user_id` — l'identité est celle portée par le JWT, le
-- schéma reste découplé de la stratégie d'identité.
--
-- Colonnes **nullables** : les lignes antérieures n'ont pas de propriétaire
-- connu ; l'API les traite comme accessibles aux seuls super-admins (`*`).
ALTER TABLE workflows ADD COLUMN owner_id TEXT;
CREATE INDEX idx_workflows_owner_id ON workflows (owner_id, id);

-- Propriétaire du run = propriétaire du workflow au dispatch. Porté par le run
-- lui-même : un workflow supprimé puis recréé sous le même id par un autre
-- utilisateur ne lui donne pas accès aux runs de l'ancien propriétaire.
ALTER TABLE runs ADD COLUMN owner_id TEXT;
CREATE INDEX idx_runs_workflow_owner ON runs (workflow_id, owner_id, created_at_ms DESC);

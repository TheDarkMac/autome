-- Runs antérieurs à la migration 0010 dans l'espace legacy (#269).
--
-- 0014 a rangé les workflows sans propriétaire dans l'espace `''`
-- (`LEGACY_NAMESPACE`) sans toucher aux runs : ceux d'avant 0010 gardaient
-- `owner_id IS NULL`, alors que la liste des runs d'un workflow filtre par
-- égalité sur l'espace (`owner_id = ''` pour le super-admin, `?owner=`). Ils
-- n'apparaissaient dans aucune liste, lisibles par id seulement, tandis que
-- les runs lancés depuis sur le même workflow (`owner_id = ''`) y figuraient.
--
-- Un seul état pour « sans propriétaire » : `''`, comme pour les workflows.
-- L'accès ne change pas : aucun `claims.sub` n'est vide, l'espace reste
-- réservé aux super-admins. `idx_runs_workflow_owner` (0010) sert toujours la
-- liste.
UPDATE runs SET owner_id = '' WHERE owner_id IS NULL;
ALTER TABLE runs ALTER COLUMN owner_id SET NOT NULL;

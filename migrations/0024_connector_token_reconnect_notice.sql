-- Invalidation signalée au propriétaire par email (#265).
--
-- Le worker pose `invalidated_at_ms` quand le provider refuse le refresh
-- token (0018). L'API envoie alors un email au propriétaire, une seule fois
-- par invalidation, même avec plusieurs répliques : elle réclame les lignes
-- dont l'invalidation n'est pas encore notifiée en recopiant sa date dans
-- `reconnect_notified_at_ms` (`UPDATE … FOR UPDATE SKIP LOCKED … RETURNING`).
--
-- Une reconnexion efface `invalidated_at_ms` ; une révocation ultérieure pose
-- une autre date, différente de la dernière notifiée : nouvel email. Rien à
-- remettre à zéro.
--
-- Les credentials déjà invalidées au déploiement sont notifiées une fois :
-- leurs workflows échouent toujours.
ALTER TABLE connector_tokens
    ADD COLUMN reconnect_notified_at_ms BIGINT NULL;

-- Lecture des invalidations à notifier, les plus anciennes d'abord : l'index
-- ne couvre que les lignes en attente.
CREATE INDEX idx_connector_tokens_reconnect_pending
    ON connector_tokens (invalidated_at_ms)
    WHERE invalidated_at_ms IS NOT NULL
      AND reconnect_notified_at_ms IS DISTINCT FROM invalidated_at_ms;

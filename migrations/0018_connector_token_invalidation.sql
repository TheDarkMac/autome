-- Credential de connecteur invalidée par le provider (#211).
--
-- Quand le endpoint token refuse le refresh token (`invalid_grant`, RFC 6749
-- §5.2 : révoqué, expiré ou déjà consommé), le worker pose cette marque au
-- lieu de représenter le même refresh token à chaque run. Les runs suivants
-- échouent sans appeler le provider, en demandant une reconnexion ; la
-- reconnexion (`/credentials/{provider}/callback` ⇒ upsert) remet les deux
-- colonnes à NULL, tout comme un rafraîchissement réussi.
--
-- `invalid_reason` : réponse d'erreur du provider (tronquée), jamais un token.
-- Les deux colonnes sont nulles ensemble ou renseignées ensemble.
ALTER TABLE connector_tokens
    ADD COLUMN invalidated_at_ms BIGINT NULL,
    ADD COLUMN invalid_reason    TEXT   NULL,
    ADD CONSTRAINT connector_tokens_invalidation_check
        CHECK ((invalidated_at_ms IS NULL) = (invalid_reason IS NULL));

-- Journal d'audit (dette #107, constat 2 ; F6.6).
-- Backe `AuditSink` côté PostgreSQL : append-only, lecture en ordre
-- d'insertion via `id`. Format plat (export SIEM/SQL).
CREATE TABLE audit_events (
    -- Ordre d'insertion stable (FIFO) — clé de lecture de `list()`.
    id           BIGSERIAL PRIMARY KEY,
    -- Horodatage fourni par le caller (clock injectable), ms Unix.
    timestamp_ms BIGINT NOT NULL,
    -- Acteur (`claims.sub`, ou `anonymous`).
    actor        TEXT   NOT NULL,
    -- Action stable `<domain>.<verb>`.
    action       TEXT   NOT NULL,
    -- Ressource ciblée (nullable).
    resource     TEXT,
    -- Résultat : success | failure | denied.
    outcome      TEXT   NOT NULL,
    -- Métadonnées libres (petit JSON).
    metadata     JSONB  NOT NULL DEFAULT 'null'::jsonb
);

-- Requêtes d'audit fréquentes : par acteur et par fenêtre temporelle.
CREATE INDEX idx_audit_events_actor ON audit_events (actor);
CREATE INDEX idx_audit_events_ts    ON audit_events (timestamp_ms);

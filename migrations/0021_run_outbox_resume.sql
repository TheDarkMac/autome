-- Outbox du dispatch et reprise des runs interrompus (#239).
--
-- `run_outbox` : message de file d'un run, écrit dans la **même transaction**
-- que le run `queued` (pattern Outbox). `published_at_ms` nul ⇒ à publier :
-- le dispatcher publie tout de suite, un relais du worker republie ce qui
-- reste en attente (broker coupé, process tué entre l'écriture et l'envoi).
-- Le message est gardé jusqu'à la fin du run : une reprise le republie
-- (remis à « non publié », `generation` incrémentée) avec une clé
-- d'idempotence propre à la génération. Marquer un message publié exige sa
-- génération : un relais en retard ne masque pas une republication.
-- Il porte le payload du déclencheur : supprimé avec le run (effacement de
-- compte, #229) et dès que le run est terminé.
--
-- `runs.attempt` : numéro de la tentative en cours, incrémenté à chaque prise
-- en charge (jeton de fencing). Une écriture de progression ou d'état final
-- n'aboutit que si la tentative n'a pas changé : un worker « zombie » (perte
-- réseau, run repris ailleurs) ne peut plus rien écrire.
--
-- `runs.heartbeat_at_ms` : battement du worker qui exécute le run ; un run
-- `running` dont le battement s'est arrêté est remis en file.
--
-- `runs.resume_state` : curseur du moteur (nœuds traités, contexte, branches
-- actives) enregistré après chaque nœud ; `next_node` nomme le nœud en cours,
-- écrit avant son exécution.
CREATE TABLE run_outbox (
    run_id          TEXT   PRIMARY KEY REFERENCES runs (id) ON DELETE CASCADE,
    queue           TEXT   NOT NULL,
    generation      INTEGER NOT NULL DEFAULT 0,
    headers         JSONB  NOT NULL,
    body            JSONB  NOT NULL,
    created_at_ms   BIGINT NOT NULL,
    published_at_ms BIGINT
);
CREATE INDEX idx_run_outbox_pending ON run_outbox (created_at_ms)
    WHERE published_at_ms IS NULL;

ALTER TABLE runs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN heartbeat_at_ms BIGINT;
ALTER TABLE runs ADD COLUMN resume_state JSONB;
CREATE INDEX idx_runs_heartbeat ON runs (heartbeat_at_ms)
    WHERE status = 'running';

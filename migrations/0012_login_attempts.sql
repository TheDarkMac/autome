-- Anti-bruteforce de la connexion partagé entre répliques de l'API (#182).
--
-- Une ligne par email tenté (fenêtre fixe) : `attempts` tentatives depuis
-- `window_start` (secondes Unix). Le compteur était in-process : avec N
-- répliques, un attaquant disposait de N fois le quota, remis à zéro à chaque
-- redémarrage.
--
-- `email_hash` = SHA-256 (hex) de l'email normalisé : on ne conserve pas les
-- adresses arbitraires soumises par un attaquant, et la clé reste bornée.
CREATE TABLE login_attempts (
    email_hash   TEXT   PRIMARY KEY CHECK (length(email_hash) = 64),
    attempts     BIGINT NOT NULL CHECK (attempts >= 1),
    window_start BIGINT NOT NULL
);
-- Purge des fenêtres expirées (tâche de maintenance de l'API).
CREATE INDEX idx_login_attempts_window ON login_attempts (window_start);

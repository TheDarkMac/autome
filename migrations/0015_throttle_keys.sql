-- Compteurs anti-bruteforce génériques : email **et** adresse IP (#190).
--
-- `login_attempts` ne comptait que par email (0012). Elle porte désormais
-- plusieurs compteurs, distingués par une clé préfixée construite par l'API :
-- `email:<email>`, `login-ip:<hmac>`, `signup-ip:<hmac>`. La table ne stocke
-- toujours que le SHA-256 de la clé : ni email ni adresse IP en clair (l'IP est
-- en outre pseudonymisée par un HMAC à clé secrète avant hachage).
--
-- Les compteurs existants (clé = email sans préfixe) deviennent orphelins et
-- sont purgés par la maintenance avec leur fenêtre (≤ 15 min).
ALTER TABLE login_attempts RENAME COLUMN email_hash TO key_hash;
ALTER TABLE login_attempts RENAME CONSTRAINT login_attempts_email_hash_check TO login_attempts_key_hash_check;

-- Une connexion réussie rend sa tentative au compteur de son IP : le compteur
-- peut redescendre à zéro.
ALTER TABLE login_attempts DROP CONSTRAINT login_attempts_attempts_check;
ALTER TABLE login_attempts ADD CONSTRAINT login_attempts_attempts_check CHECK (attempts >= 0);

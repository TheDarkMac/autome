-- Vérification de l'adresse email (#231).
--
-- `email_verified_at` non nul ⇒ la possession de l'adresse est prouvée (lien
-- reçu par email, ou réinitialisation du mot de passe). Quand le déploiement
-- exige cette preuve, l'API refuse la connexion des comptes non vérifiés.
--
-- Les comptes antérieurs sont tenus pour vérifiés (date de création) : la
-- migration ne doit fermer la porte à personne.
ALTER TABLE users ADD COLUMN email_verified_at TIMESTAMPTZ NULL;
UPDATE users SET email_verified_at = created_at;

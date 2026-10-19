//! Jetons de réinitialisation du mot de passe (#204 — mot de passe oublié).
//!
//! Un jeton est **sans état** : `<user_id>.<expiration>.<mac>` où
//! `mac = HMAC-SHA256(clé, user_id ‖ expiration ‖ hash actuel du mot de passe)`,
//! la clé étant dérivée d'`ENCRYPTION_KEY` (identique sur toutes les
//! répliques). Conséquences :
//!
//! - rien à stocker ni à purger, et la base seule ne permet pas d'en forger ;
//! - **usage unique** par construction : la réinitialisation remplace le hash
//!   (Argon2 salé, donc toujours différent), ce qui invalide le jeton — et
//!   l'écriture conditionnelle `UserStore::update_password` départage deux
//!   utilisations concurrentes ;
//! - tout changement de mot de passe (y compris par `/auth/password`)
//!   invalide les liens en circulation ;
//! - durée de vie bornée ([`RESET_TTL_SECONDS`]).
//!
//! Le lien envoyé porte le jeton dans le **fragment** (`/#reset=…`) : le
//! navigateur ne l'envoie ni au serveur qui sert le frontend (journaux
//! d'accès) ni dans l'en-tête `Referer`.

use autome_core::Config;

use crate::mail::{MailSettings, OutgoingMail};
use crate::signed_link::{LinkTokens, ParsedLinkToken};

/// Durée de validité d'un lien de réinitialisation (30 min).
pub const RESET_TTL_SECONDS: i64 = 30 * 60;
/// Demandes de réinitialisation tolérées par email et par fenêtre
/// d'anti-bruteforce : borne les emails envoyés à une même boîte.
pub const DEFAULT_RESET_EMAIL_MAX_REQUESTS: u32 = 3;
/// Objet de l'email de réinitialisation.
pub const RESET_MAIL_SUBJECT: &str = "Réinitialisation de votre mot de passe Autome";
/// Préfixe de domaine de la clé des jetons de réinitialisation.
const RESET_DOMAIN: &str = "autome/password-reset/v1";

/// Émission et vérification des jetons de réinitialisation : jetons liés au
/// hash actuel du mot de passe (cf. [`crate::signed_link`]).
#[derive(Clone)]
pub struct ResetTokens(LinkTokens);

impl std::fmt::Debug for ResetTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResetTokens(<redacted>)")
    }
}

/// Jeton découpé, pas encore vérifié.
pub type ParsedResetToken<'a> = ParsedLinkToken<'a>;

impl ResetTokens {
    /// Clé dérivée de `secret` (séparation de domaine : elle diffère de toute
    /// autre clé tirée du même secret).
    pub fn new(secret: &[u8]) -> Self {
        Self(LinkTokens::new(RESET_DOMAIN, secret, RESET_TTL_SECONDS))
    }

    /// Clé dérivée d'`ENCRYPTION_KEY`. La changer invalide les liens en cours.
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.encryption_key.expose().as_bytes())
    }

    /// Jeton pour le compte `user_id` dont le hash actuel est `password_hash`,
    /// valable [`RESET_TTL_SECONDS`] à partir de `now`.
    pub fn issue(&self, user_id: &str, password_hash: &str, now: i64) -> String {
        self.0.issue(user_id, password_hash, now)
    }

    /// Découpe `token` sans le vérifier ; `None` s'il est mal formé.
    pub fn parse(token: &str) -> Option<ParsedResetToken<'_>> {
        LinkTokens::parse(token)
    }

    /// `true` si `token` est authentique, non expiré à `now`, et émis pour le
    /// hash **actuel** `password_hash` du compte (comparaison à temps
    /// constant).
    pub fn verify(&self, token: &ParsedResetToken<'_>, password_hash: &str, now: i64) -> bool {
        self.0.verify(token, password_hash, now)
    }
}

/// Réinitialisation du mot de passe : jetons, envoi et quotas.
#[derive(Clone, Debug)]
pub struct PasswordReset {
    /// Jetons de réinitialisation.
    pub tokens: ResetTokens,
    /// Transport des emails et URL publique du frontend.
    pub mail: MailSettings,
    /// Demandes tolérées par email et par fenêtre.
    pub email_max: u32,
}

impl PasswordReset {
    /// Réinitialisation avec le quota par email par défaut.
    pub fn new(tokens: ResetTokens, mail: MailSettings) -> Self {
        Self {
            tokens,
            mail,
            email_max: DEFAULT_RESET_EMAIL_MAX_REQUESTS,
        }
    }

    /// Lien de réinitialisation : jeton dans le fragment.
    pub fn link(&self, token: &str) -> String {
        format!("{}/#reset={token}", self.mail.public_url)
    }

    /// Email de réinitialisation pour `to`.
    pub fn mail_for(&self, to: &str, token: &str) -> OutgoingMail {
        let minutes = RESET_TTL_SECONDS / 60;
        OutgoingMail {
            to: to.to_string(),
            subject: RESET_MAIL_SUBJECT.to_string(),
            body: format!(
                "Bonjour,\n\n\
                 Une réinitialisation du mot de passe de votre compte Autome a été \
                 demandée. Pour choisir un nouveau mot de passe, ouvrez ce lien \
                 (valable {minutes} minutes, utilisable une seule fois) :\n\n\
                 {}\n\n\
                 Toutes vos sessions ouvertes seront fermées.\n\n\
                 Si vous n'êtes pas à l'origine de cette demande, ignorez ce message : \
                 votre mot de passe reste inchangé.\n",
                self.link(token)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::mail::MemoryMailer;

    const UID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const NOW: i64 = 1_800_000_000;

    fn tokens() -> ResetTokens {
        ResetTokens::new(b"encryption-key")
    }

    #[test]
    fn issued_token_verifies_until_expiry() {
        let t = tokens();
        let token = t.issue(UID, "hash-v1", NOW);
        let parsed = ResetTokens::parse(&token).expect("bien formé");
        assert_eq!(parsed.user_id, UID);
        assert_eq!(parsed.expires_at, NOW + RESET_TTL_SECONDS);
        assert!(t.verify(&parsed, "hash-v1", NOW));
        assert!(t.verify(&parsed, "hash-v1", NOW + RESET_TTL_SECONDS - 1));
        assert!(!t.verify(&parsed, "hash-v1", NOW + RESET_TTL_SECONDS));
    }

    #[test]
    fn token_dies_with_the_password_hash() {
        let t = tokens();
        let token = t.issue(UID, "hash-v1", NOW);
        let parsed = ResetTokens::parse(&token).unwrap();
        assert!(
            !t.verify(&parsed, "hash-v2", NOW),
            "mot de passe changé depuis"
        );
    }

    #[test]
    fn forged_or_tampered_tokens_are_refused() {
        let t = tokens();
        let token = t.issue(UID, "hash-v1", NOW);

        // Autre clé (autre ENCRYPTION_KEY).
        let other = ResetTokens::new(b"other-key");
        assert!(!other.verify(&ResetTokens::parse(&token).unwrap(), "hash-v1", NOW));

        // Expiration repoussée par l'attaquant : le MAC ne correspond plus.
        let mut parts: Vec<&str> = token.split('.').collect();
        let later = (NOW + 10 * RESET_TTL_SECONDS).to_string();
        parts[1] = &later;
        let tampered = parts.join(".");
        assert!(!t.verify(&ResetTokens::parse(&tampered).unwrap(), "hash-v1", NOW));

        // Jeton d'un compte réutilisé pour un autre.
        let swapped = token.replacen(UID, "00000000-0000-0000-0000-000000000001", 1);
        assert!(!t.verify(&ResetTokens::parse(&swapped).unwrap(), "hash-v1", NOW));
    }

    #[test]
    fn malformed_tokens_do_not_parse() {
        let mac = "a".repeat(64);
        for bad in [
            String::new(),
            "abc".into(),
            format!("{UID}.{NOW}"),
            format!("{UID}.{NOW}.{mac}.x"),
            format!(".{NOW}.{mac}"),
            format!("{}.{NOW}.{mac}", "a".repeat(65)),
            format!("alice\nforged.{NOW}.{mac}"),
            format!("{UID}.demain.{mac}"),
            format!("{UID}.{NOW}.{}", "a".repeat(63)),
            format!("{UID}.{NOW}.{}", "g".repeat(64)),
            format!("{UID}.{NOW}.{}é", "a".repeat(62)),
        ] {
            assert!(
                ResetTokens::parse(&bad).is_none(),
                "devrait refuser {bad:?}"
            );
        }
    }

    #[test]
    fn keys_are_domain_separated_and_redacted() {
        // Même secret, préfixe des webhooks (#202) : clé différente.
        let mut webhook = Sha256::new();
        webhook.update(b"autome/webhook/v1\0");
        webhook.update(b"encryption-key");
        let webhook: [u8; 32] = webhook.finalize().into();
        assert_ne!(tokens().0.key, webhook);
        assert_ne!(
            tokens().0.key,
            <[u8; 32]>::from(Sha256::digest(b"encryption-key"))
        );
        assert_eq!(format!("{:?}", tokens()), "ResetTokens(<redacted>)");
    }

    #[test]
    fn mail_carries_the_link_in_the_fragment() {
        let reset = PasswordReset::new(
            tokens(),
            MailSettings {
                mailer: Arc::new(MemoryMailer::new()),
                public_url: "https://app.example.io".into(),
            },
        );
        let mail = reset.mail_for("alice@example.io", "TOKEN");
        assert_eq!(mail.to, "alice@example.io");
        assert_eq!(mail.subject, RESET_MAIL_SUBJECT);
        assert!(mail.body.contains("https://app.example.io/#reset=TOKEN"));
        assert!(mail.body.contains("30 minutes"));
    }
}

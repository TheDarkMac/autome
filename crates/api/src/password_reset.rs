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
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::mail::{MailSettings, OutgoingMail};

/// Durée de validité d'un lien de réinitialisation (30 min).
pub const RESET_TTL_SECONDS: i64 = 30 * 60;
/// Demandes de réinitialisation tolérées par email et par fenêtre
/// d'anti-bruteforce : borne les emails envoyés à une même boîte.
pub const DEFAULT_RESET_EMAIL_MAX_REQUESTS: u32 = 3;
/// Objet de l'email de réinitialisation.
pub const RESET_MAIL_SUBJECT: &str = "Réinitialisation de votre mot de passe Autome";

/// Émission et vérification des jetons de réinitialisation.
#[derive(Clone)]
pub struct ResetTokens {
    key: [u8; 32],
}

impl std::fmt::Debug for ResetTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResetTokens(<redacted>)")
    }
}

/// Jeton découpé, pas encore vérifié.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedResetToken<'a> {
    /// Compte visé (`users.id`).
    pub user_id: &'a str,
    /// Expiration (secondes Unix).
    pub expires_at: i64,
    mac: Vec<u8>,
}

impl ResetTokens {
    /// Clé dérivée de `secret` (séparation de domaine : elle diffère de toute
    /// autre clé tirée du même secret).
    pub fn new(secret: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"autome/password-reset/v1\0");
        hasher.update(secret);
        Self {
            key: hasher.finalize().into(),
        }
    }

    /// Clé dérivée d'`ENCRYPTION_KEY`. La changer invalide les liens en cours.
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.encryption_key.expose().as_bytes())
    }

    fn mac(&self, user_id: &str, expires_at: i64, password_hash: &str) -> Hmac<Sha256> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepte toute longueur");
        // Champs séparés par un octet nul : aucune concaténation ambiguë.
        mac.update(user_id.as_bytes());
        mac.update(b"\0");
        mac.update(expires_at.to_string().as_bytes());
        mac.update(b"\0");
        mac.update(password_hash.as_bytes());
        mac
    }

    /// Jeton pour le compte `user_id` dont le hash actuel est `password_hash`,
    /// valable [`RESET_TTL_SECONDS`] à partir de `now`.
    pub fn issue(&self, user_id: &str, password_hash: &str, now: i64) -> String {
        let expires_at = now.saturating_add(RESET_TTL_SECONDS);
        let mac = self
            .mac(user_id, expires_at, password_hash)
            .finalize()
            .into_bytes();
        format!("{user_id}.{expires_at}.{}", hex(&mac))
    }

    /// Découpe `token` sans le vérifier ; `None` s'il est mal formé.
    pub fn parse(token: &str) -> Option<ParsedResetToken<'_>> {
        let mut parts = token.split('.');
        let (user_id, expires_at, mac) = (parts.next()?, parts.next()?, parts.next()?);
        // L'identifiant vient d'une saisie non fiable et finit dans l'audit :
        // forme d'un `users.id` (UUID) seulement.
        let id_ok = !user_id.is_empty()
            && user_id.len() <= 64
            && user_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if parts.next().is_some() || !id_ok {
            return None;
        }
        Some(ParsedResetToken {
            user_id,
            expires_at: expires_at.parse().ok()?,
            mac: unhex(mac)?,
        })
    }

    /// `true` si `token` est authentique, non expiré à `now`, et émis pour le
    /// hash **actuel** `password_hash` du compte (comparaison à temps
    /// constant).
    pub fn verify(&self, token: &ParsedResetToken<'_>, password_hash: &str, now: i64) -> bool {
        token.expires_at > now
            && self
                .mac(token.user_id, token.expires_at, password_hash)
                .verify_slice(&token.mac)
                .is_ok()
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
        assert_ne!(tokens().key, webhook);
        assert_ne!(
            tokens().key,
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

//! Vérification de l'adresse email (#231).
//!
//! Quand elle est active, un compte créé par `/auth/signup` n'est **pas**
//! vérifié : l'API lui envoie un lien et refuse sa connexion (403) tant qu'il
//! n'a pas été ouvert. Le lien porte un jeton sans état (cf.
//! [`crate::signed_link`]) lié à l'**email** du compte, valable
//! [`VERIFY_TTL_SECONDS`] : rien à stocker, et un lien émis pour une adresse
//! que le compte n'a plus ne vérifie rien. Comme pour la réinitialisation du
//! mot de passe, le jeton voyage dans le **fragment** (`/#verify=…`) : ni
//! journaux d'accès du frontend, ni en-tête `Referer`.
//!
//! Elle est active dès qu'un envoi d'email est possible (SMTP, ou journal en
//! `dev`), sauf `AUTOME_EMAIL_VERIFICATION=off`. Sans envoi possible (`prod`
//! sans SMTP), exiger la vérification fermerait la porte à toute inscription :
//! les comptes sont alors créés vérifiés.

use autome_core::{normalize_email, AuditEvent, AuditOutcome, AuditSink, Config, UserStore};

use crate::mail::{MailSettings, OutgoingMail};
use crate::signed_link::{LinkTokens, ParsedLinkToken};

/// Durée de validité d'un lien de vérification (24 h : il part à
/// l'inscription, la personne ne l'ouvre pas forcément tout de suite).
pub const VERIFY_TTL_SECONDS: i64 = 24 * 3600;
/// Liens de vérification tolérés par email et par fenêtre d'anti-bruteforce
/// (inscription comprise) : borne les emails envoyés à une même boîte.
pub const DEFAULT_VERIFY_EMAIL_MAX_REQUESTS: u32 = 3;
/// Objet de l'email de vérification.
pub const VERIFY_MAIL_SUBJECT: &str = "Confirmez votre adresse email Autome";
/// Préfixe de domaine de la clé des jetons de vérification.
const VERIFY_DOMAIN: &str = "autome/email-verification/v1";

/// Émission et vérification des jetons de vérification d'email.
#[derive(Clone)]
pub struct VerifyTokens(LinkTokens);

impl std::fmt::Debug for VerifyTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VerifyTokens(<redacted>)")
    }
}

impl VerifyTokens {
    /// Clé dérivée de `secret`, séparée de celle des autres liens.
    pub fn new(secret: &[u8]) -> Self {
        Self(LinkTokens::new(VERIFY_DOMAIN, secret, VERIFY_TTL_SECONDS))
    }

    /// Clé dérivée d'`ENCRYPTION_KEY`. La changer invalide les liens en cours.
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.encryption_key.expose().as_bytes())
    }

    /// Jeton pour le compte `user_id` d'adresse `email`, valable
    /// [`VERIFY_TTL_SECONDS`] à partir de `now`.
    pub fn issue(&self, user_id: &str, email: &str, now: i64) -> String {
        self.0.issue(user_id, email, now)
    }

    /// Découpe `token` sans le vérifier ; `None` s'il est mal formé.
    pub fn parse(token: &str) -> Option<ParsedLinkToken<'_>> {
        LinkTokens::parse(token)
    }

    /// `true` si `token` est authentique, non expiré à `now`, et émis pour
    /// l'adresse **actuelle** `email` du compte.
    pub fn verify(&self, token: &ParsedLinkToken<'_>, email: &str, now: i64) -> bool {
        self.0.verify(token, email, now)
    }
}

/// Vérification de l'email : jetons, envoi et quotas.
#[derive(Clone, Debug)]
pub struct EmailVerification {
    /// Jetons de vérification.
    pub tokens: VerifyTokens,
    /// Transport des emails et URL publique du frontend.
    pub mail: MailSettings,
    /// Liens tolérés par email et par fenêtre.
    pub email_max: u32,
}

impl EmailVerification {
    /// Vérification avec le quota par email par défaut.
    pub fn new(tokens: VerifyTokens, mail: MailSettings) -> Self {
        Self {
            tokens,
            mail,
            email_max: DEFAULT_VERIFY_EMAIL_MAX_REQUESTS,
        }
    }

    /// Lien de vérification : jeton dans le fragment.
    pub fn link(&self, token: &str) -> String {
        format!("{}/#verify={token}", self.mail.public_url)
    }

    /// Email de vérification pour `to`.
    pub fn mail_for(&self, to: &str, token: &str) -> OutgoingMail {
        let hours = VERIFY_TTL_SECONDS / 3600;
        OutgoingMail {
            to: to.to_string(),
            subject: VERIFY_MAIL_SUBJECT.to_string(),
            body: format!(
                "Bonjour,\n\n\
                 Un compte Autome a été créé avec cette adresse. Pour la confirmer \
                 et pouvoir vous connecter, ouvrez ce lien (valable {hours} heures) :\n\n\
                 {}\n\n\
                 Si vous n'êtes pas à l'origine de cette inscription, ignorez ce \
                 message : sans confirmation, personne ne peut se connecter à ce \
                 compte.\n",
                self.link(token)
            ),
        }
    }
}

/// Valeur de `AUTOME_EMAIL_VERIFICATION` : `required` (défaut) ou `off`.
pub fn verification_required_from_source(raw: Option<&str>) -> Result<bool, String> {
    match raw.map(str::trim).unwrap_or_default() {
        "" | "required" => Ok(true),
        "off" => Ok(false),
        other => Err(format!(
            "AUTOME_EMAIL_VERIFICATION : `required` ou `off` attendu, obtenu `{other}`"
        )),
    }
}

/// Issue de [`verify_email_by_operator`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorVerification {
    /// Adresse marquée vérifiée (`already` : elle l'était déjà).
    Verified {
        /// Identifiant du compte.
        user_id: String,
        /// L'adresse était déjà vérifiée.
        already: bool,
    },
    /// Aucun compte pour cet email.
    UnknownEmail,
}

/// Hors API (`autome-api verify-email <email>`) : marque vérifiée l'adresse
/// du compte `email`, par exemple quand l'envoi d'emails est en panne ou pour
/// les tests de bout en bout. Exige l'accès à l'infrastructure ; audité
/// `admin.user_verify_email` sous l'acteur `cli`.
pub async fn verify_email_by_operator(
    users: &(dyn UserStore + Send + Sync),
    audit: &(dyn AuditSink + Send + Sync),
    email: &str,
    now_ms: i64,
) -> autome_core::Result<OperatorVerification> {
    let email = normalize_email(email);
    let Some(account) = users.find_by_email(&email).await? else {
        return Ok(OperatorVerification::UnknownEmail);
    };
    if !users
        .mark_email_verified(&account.id, &account.email)
        .await?
    {
        return Ok(OperatorVerification::UnknownEmail);
    }
    let event = AuditEvent::new(
        now_ms,
        "cli",
        "admin.user_verify_email",
        AuditOutcome::Success,
    )
    .with_resource(&account.id);
    if let Err(e) = audit.record(event).await {
        tracing::error!(error = %e, "événement d'audit non enregistré");
    }
    Ok(OperatorVerification::Verified {
        user_id: account.id,
        already: account.email_verified,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use autome_core::{CreateUser, InMemoryAuditSink, InMemoryPermissionStore, InMemoryUserStore};

    use super::*;
    use crate::mail::MemoryMailer;
    use crate::password_reset::ResetTokens;

    const UID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const NOW: i64 = 1_800_000_000;

    fn tokens() -> VerifyTokens {
        VerifyTokens::new(b"encryption-key")
    }

    #[test]
    fn token_is_bound_to_the_address_and_expires() {
        let t = tokens();
        let token = t.issue(UID, "alice@example.io", NOW);
        let parsed = VerifyTokens::parse(&token).expect("bien formé");
        assert_eq!(parsed.user_id, UID);
        assert_eq!(parsed.expires_at, NOW + VERIFY_TTL_SECONDS);
        assert!(t.verify(&parsed, "alice@example.io", NOW + VERIFY_TTL_SECONDS - 1));
        assert!(!t.verify(&parsed, "alice@example.io", NOW + VERIFY_TTL_SECONDS));
        assert!(!t.verify(&parsed, "other@example.io", NOW), "autre adresse");
        assert!(!VerifyTokens::new(b"other-key").verify(&parsed, "alice@example.io", NOW));
        assert_eq!(format!("{t:?}"), "VerifyTokens(<redacted>)");
    }

    #[test]
    fn reset_and_verification_tokens_are_not_interchangeable() {
        let reset = ResetTokens::new(b"encryption-key");
        // Même compte, même valeur liée : seule la clé (domaine) diffère.
        let token = reset.issue(UID, "alice@example.io", NOW);
        let parsed = VerifyTokens::parse(&token).unwrap();
        assert!(!tokens().verify(&parsed, "alice@example.io", NOW));
        let token = tokens().issue(UID, "hash", NOW);
        assert!(!reset.verify(&ResetTokens::parse(&token).unwrap(), "hash", NOW));
    }

    #[test]
    fn mail_carries_the_link_in_the_fragment() {
        let verification = EmailVerification::new(
            tokens(),
            MailSettings {
                mailer: Arc::new(MemoryMailer::new()),
                public_url: "https://app.example.io".into(),
            },
        );
        let mail = verification.mail_for("alice@example.io", "TOKEN");
        assert_eq!(mail.to, "alice@example.io");
        assert_eq!(mail.subject, VERIFY_MAIL_SUBJECT);
        assert!(mail.body.contains("https://app.example.io/#verify=TOKEN"));
        assert!(mail.body.contains("24 heures"));
    }

    #[test]
    fn verification_setting_is_parsed_strictly() {
        assert_eq!(verification_required_from_source(None), Ok(true));
        assert_eq!(verification_required_from_source(Some(" ")), Ok(true));
        assert_eq!(
            verification_required_from_source(Some("required")),
            Ok(true)
        );
        assert_eq!(verification_required_from_source(Some("off")), Ok(false));
        assert!(verification_required_from_source(Some("false"))
            .unwrap_err()
            .contains("AUTOME_EMAIL_VERIFICATION"));
    }

    #[tokio::test]
    async fn operator_verification_marks_the_account_and_audits() {
        let users = InMemoryUserStore::new(Arc::new(InMemoryPermissionStore::new()));
        let audit = InMemoryAuditSink::new();
        let CreateUser::Created(alice) = users
            .create("alice@example.io", "h", &HashSet::new(), false)
            .await
            .unwrap()
        else {
            panic!("création");
        };
        assert_eq!(
            verify_email_by_operator(&users, &audit, " Alice@Example.io ", 1)
                .await
                .unwrap(),
            OperatorVerification::Verified {
                user_id: alice.id.clone(),
                already: false
            }
        );
        assert!(
            users
                .find_by_id(&alice.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        assert_eq!(
            verify_email_by_operator(&users, &audit, "alice@example.io", 2)
                .await
                .unwrap(),
            OperatorVerification::Verified {
                user_id: alice.id.clone(),
                already: true
            }
        );
        assert_eq!(
            verify_email_by_operator(&users, &audit, "nobody@example.io", 3)
                .await
                .unwrap(),
            OperatorVerification::UnknownEmail
        );
        let events = audit.list().await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|e| e.actor == "cli" && e.action == "admin.user_verify_email"));
        assert_eq!(events[0].resource.as_deref(), Some(alice.id.as_str()));
    }
}

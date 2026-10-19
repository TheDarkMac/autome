//! Invitation de compte par un administrateur (#241).
//!
//! L'administration crée le compte (rôle choisi) **sans mot de passe
//! utilisable** ([`autome_core::unusable_password_hash`]) et envoie un lien à
//! son adresse : la personne invitée y choisit son mot de passe et se trouve
//! connectée. Tant qu'elle ne l'a pas fait, aucune connexion n'est possible et
//! l'administration voit une « invitation en attente ».
//!
//! Le lien porte un jeton sans état (cf. [`crate::signed_link`]) sous un
//! domaine propre, lié au hash inutilisable **actuel** du compte et valable
//! [`INVITE_TTL_SECONDS`] :
//!
//! - usage unique par construction : l'acceptation remplace le hash
//!   (écriture conditionnelle, deux acceptations concurrentes ont un seul
//!   gagnant) ;
//! - renvoyer l'invitation tire un nouveau hash inutilisable : les liens
//!   précédents ne valent plus rien ;
//! - un jeton de réinitialisation ou de vérification n'est jamais accepté ici
//!   (autre clé), et inversement.
//!
//! Comme les autres liens, le jeton voyage dans le **fragment**
//! (`/#invite=…`) : ni journaux d'accès du frontend, ni en-tête `Referer`.
//!
//! Les inscriptions publiques peuvent être fermées
//! (`AUTOME_SIGNUP=invite_only`, cf. [`signup_open_from_source`]) : les comptes
//! ne naissent alors que par invitation. Une instance fermée dès le départ
//! s'amorce par `autome-api invite <email> admin` (hors HTTP).

use std::collections::HashSet;

use autome_core::{
    normalize_email, unusable_password_hash, validate_email, AuditEvent, AuditOutcome, AuditSink,
    Config, CreateUser, Role, UserRecord, UserStore,
};

use crate::mail::{MailSettings, OutgoingMail};
use crate::signed_link::{LinkTokens, ParsedLinkToken};

/// Durée de validité d'un lien d'invitation (7 jours : la personne invitée
/// n'attend pas forcément ce message).
pub const INVITE_TTL_SECONDS: i64 = 7 * 24 * 3600;
/// Objet de l'email d'invitation.
pub const INVITE_MAIL_SUBJECT: &str = "Invitation à rejoindre Autome";
/// Préfixe de domaine de la clé des jetons d'invitation.
const INVITE_DOMAIN: &str = "autome/invitation/v1";

/// Émission et vérification des jetons d'invitation : jetons liés au hash
/// inutilisable actuel du compte.
#[derive(Clone)]
pub struct InviteTokens(LinkTokens);

impl std::fmt::Debug for InviteTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InviteTokens(<redacted>)")
    }
}

impl InviteTokens {
    /// Clé dérivée de `secret`, séparée de celle des autres liens.
    pub fn new(secret: &[u8]) -> Self {
        Self(LinkTokens::new(INVITE_DOMAIN, secret, INVITE_TTL_SECONDS))
    }

    /// Clé dérivée d'`ENCRYPTION_KEY`. La changer invalide les liens en cours.
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.encryption_key.expose().as_bytes())
    }

    /// Jeton pour le compte `user_id` dont le hash (inutilisable) actuel est
    /// `password_hash`, valable [`INVITE_TTL_SECONDS`] à partir de `now`.
    pub fn issue(&self, user_id: &str, password_hash: &str, now: i64) -> String {
        self.0.issue(user_id, password_hash, now)
    }

    /// Découpe `token` sans le vérifier ; `None` s'il est mal formé.
    pub fn parse(token: &str) -> Option<ParsedLinkToken<'_>> {
        LinkTokens::parse(token)
    }

    /// `true` si `token` est authentique, non expiré à `now`, et émis pour le
    /// hash **actuel** `password_hash` du compte.
    pub fn verify(&self, token: &ParsedLinkToken<'_>, password_hash: &str, now: i64) -> bool {
        self.0.verify(token, password_hash, now)
    }
}

/// Invitations : jetons et envoi des emails.
#[derive(Clone, Debug)]
pub struct Invitation {
    /// Jetons d'invitation.
    pub tokens: InviteTokens,
    /// Transport des emails et URL publique du frontend.
    pub mail: MailSettings,
}

impl Invitation {
    /// Invitations envoyées par `mail`.
    pub fn new(tokens: InviteTokens, mail: MailSettings) -> Self {
        Self { tokens, mail }
    }

    /// Lien d'invitation : jeton dans le fragment.
    pub fn link(&self, token: &str) -> String {
        format!("{}/#invite={token}", self.mail.public_url)
    }

    /// Email d'invitation pour `to`.
    pub fn mail_for(&self, to: &str, token: &str) -> OutgoingMail {
        let days = INVITE_TTL_SECONDS / 86_400;
        OutgoingMail {
            to: to.to_string(),
            subject: INVITE_MAIL_SUBJECT.to_string(),
            body: format!(
                "Bonjour,\n\n\
                 Un administrateur vous a créé un compte Autome avec cette adresse. \
                 Pour l'activer, choisissez votre mot de passe en ouvrant ce lien \
                 (valable {days} jours, utilisable une seule fois) :\n\n\
                 {}\n\n\
                 Si vous ne vous attendiez pas à cette invitation, ignorez ce message : \
                 sans mot de passe choisi, personne ne peut se connecter à ce compte.\n",
                self.link(token)
            ),
        }
    }

    /// Envoie un lien d'invitation à `user`, lié à son hash actuel. L'erreur
    /// est destinée aux journaux (jamais le jeton).
    pub async fn send(&self, user: &UserRecord, now: i64) -> Result<(), String> {
        let token = self.tokens.issue(&user.id, &user.password_hash, now);
        self.mail
            .mailer
            .send(&self.mail_for(&user.email, &token))
            .await
    }
}

/// Issue de [`invite_account`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invited {
    /// Compte créé ; `sent` : l'email d'invitation est parti.
    Created {
        /// Compte créé (sans mot de passe utilisable).
        user: UserRecord,
        /// L'email d'invitation a été accepté par le transport.
        sent: bool,
    },
    /// Un compte existe déjà pour cet email (rien n'a été écrit).
    EmailTaken,
}

/// Crée le compte invité `email` (déjà normalisé et validé) avec
/// `permissions`, adresse non vérifiée et sans mot de passe utilisable, puis
/// lui envoie l'invitation. Un envoi raté n'annule pas la création :
/// l'invitation peut être renvoyée.
pub async fn invite_account(
    users: &(dyn UserStore + Send + Sync),
    invitation: &Invitation,
    email: &str,
    permissions: &HashSet<String>,
    now: i64,
) -> autome_core::Result<Invited> {
    let user = match users
        .create(email, &unusable_password_hash(), permissions, false)
        .await?
    {
        CreateUser::Created(user) => user,
        CreateUser::EmailTaken => return Ok(Invited::EmailTaken),
    };
    let sent = match invitation.send(&user, now).await {
        Ok(()) => {
            tracing::info!(user_id = %user.id, "invitation envoyée");
            true
        }
        Err(e) => {
            tracing::error!(user_id = %user.id, error = %e, "envoi de l'invitation impossible");
            false
        }
    };
    Ok(Invited::Created { user, sent })
}

/// Hors API (`autome-api invite <email> [role]`) : invite `email` avec le
/// rôle standard `role` (défaut `editor`), par exemple le premier
/// administrateur d'une instance aux inscriptions fermées. Exige l'accès à
/// l'infrastructure ; audité `admin.user_invite` sous l'acteur `cli`.
pub async fn invite_by_operator(
    users: &(dyn UserStore + Send + Sync),
    audit: &(dyn AuditSink + Send + Sync),
    invitation: &Invitation,
    email: &str,
    role: Option<&str>,
    now_ms: i64,
) -> Result<Invited, String> {
    let email = normalize_email(email);
    validate_email(&email).map_err(|e| e.to_string())?;
    let role = match role {
        None => Role::editor(),
        Some(name) => Role::standard(name)
            .ok_or_else(|| format!("rôle inconnu « {name} » (admin, editor ou viewer)"))?,
    };
    let invited = invite_account(
        users,
        invitation,
        &email,
        &role.permissions,
        now_ms.div_euclid(1_000),
    )
    .await
    .map_err(|e| e.to_string())?;
    if let Invited::Created { user, sent } = &invited {
        let event = AuditEvent::new(now_ms, "cli", "admin.user_invite", AuditOutcome::Success)
            .with_resource(&user.id)
            .with_metadata(serde_json::json!({ "role": role.name, "sent": sent }));
        if let Err(e) = audit.record(event).await {
            tracing::error!(error = %e, "événement d'audit non enregistré");
        }
    }
    Ok(invited)
}

/// Valeur de `AUTOME_SIGNUP` : `open` (défaut, inscriptions publiques) ou
/// `invite_only` (comptes créés par invitation seulement). `true` = ouvertes.
pub fn signup_open_from_source(raw: Option<&str>) -> Result<bool, String> {
    match raw.map(str::trim).unwrap_or_default() {
        "" | "open" => Ok(true),
        "invite_only" => Ok(false),
        other => Err(format!(
            "AUTOME_SIGNUP : `open` ou `invite_only` attendu, obtenu `{other}`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use autome_core::{
        has_usable_password, InMemoryAuditSink, InMemoryPermissionStore, InMemoryUserStore,
    };

    use super::*;
    use crate::email_verification::VerifyTokens;
    use crate::mail::{Mailer, MemoryMailer};
    use crate::password_reset::ResetTokens;

    const UID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const NOW: i64 = 1_800_000_000;

    fn tokens() -> InviteTokens {
        InviteTokens::new(b"encryption-key")
    }

    fn invitation(mailer: Arc<dyn Mailer>) -> Invitation {
        Invitation::new(
            tokens(),
            MailSettings {
                mailer,
                public_url: "https://app.example.io".into(),
            },
        )
    }

    struct FailingMailer;

    #[async_trait]
    impl Mailer for FailingMailer {
        async fn send(&self, _: &OutgoingMail) -> Result<(), String> {
            Err("smtp en panne".into())
        }
    }

    #[test]
    fn token_is_bound_to_the_current_hash_and_expires() {
        let t = tokens();
        let token = t.issue(UID, "!invited$a", NOW);
        let parsed = InviteTokens::parse(&token).expect("bien formé");
        assert_eq!(parsed.expires_at, NOW + INVITE_TTL_SECONDS);
        assert!(t.verify(&parsed, "!invited$a", NOW + INVITE_TTL_SECONDS - 1));
        assert!(!t.verify(&parsed, "!invited$a", NOW + INVITE_TTL_SECONDS));
        assert!(!t.verify(&parsed, "!invited$b", NOW), "invitation renvoyée");
        assert!(!InviteTokens::new(b"other").verify(&parsed, "!invited$a", NOW));
        assert_eq!(format!("{t:?}"), "InviteTokens(<redacted>)");
    }

    #[test]
    fn other_link_tokens_are_not_invitations() {
        let reset = ResetTokens::new(b"encryption-key").issue(UID, "!invited$a", NOW);
        let verify = VerifyTokens::new(b"encryption-key").issue(UID, "!invited$a", NOW);
        for token in [reset, verify] {
            let parsed = InviteTokens::parse(&token).unwrap();
            assert!(!tokens().verify(&parsed, "!invited$a", NOW));
        }
        let invite = tokens().issue(UID, "h", NOW);
        let reset = ResetTokens::new(b"encryption-key");
        assert!(!reset.verify(&ResetTokens::parse(&invite).unwrap(), "h", NOW));
    }

    #[test]
    fn mail_carries_the_link_in_the_fragment() {
        let mail = invitation(Arc::new(MemoryMailer::new())).mail_for("bob@example.io", "TOKEN");
        assert_eq!(mail.to, "bob@example.io");
        assert_eq!(mail.subject, INVITE_MAIL_SUBJECT);
        assert!(mail.body.contains("https://app.example.io/#invite=TOKEN"));
        assert!(mail.body.contains("7 jours"));
    }

    #[test]
    fn signup_setting_is_parsed_strictly() {
        assert_eq!(signup_open_from_source(None), Ok(true));
        assert_eq!(signup_open_from_source(Some(" open ")), Ok(true));
        assert_eq!(signup_open_from_source(Some("invite_only")), Ok(false));
        assert!(signup_open_from_source(Some("closed"))
            .unwrap_err()
            .contains("AUTOME_SIGNUP"));
    }

    fn users() -> InMemoryUserStore {
        InMemoryUserStore::new(Arc::new(InMemoryPermissionStore::new()))
    }

    #[tokio::test]
    async fn an_invited_account_has_no_password_and_gets_a_working_link() {
        let users = users();
        let mailer = Arc::new(MemoryMailer::new());
        let inv = invitation(mailer.clone());
        let Invited::Created { user, sent } = invite_account(
            &users,
            &inv,
            "bob@example.io",
            &Role::viewer().permissions,
            NOW,
        )
        .await
        .unwrap() else {
            panic!("création");
        };
        assert!(sent);
        assert!(!has_usable_password(&user.password_hash));
        assert!(!user.email_verified);
        let summary = users.summary(&user.id).await.unwrap().unwrap();
        assert!(!summary.password_set);
        assert_eq!(summary.permissions, Role::viewer().permissions);

        let mail = mailer.sent().pop().unwrap();
        let token = mail.body.split("#invite=").nth(1).unwrap();
        let token = token.split_whitespace().next().unwrap();
        let parsed = InviteTokens::parse(token).unwrap();
        assert!(inv.tokens.verify(&parsed, &user.password_hash, NOW));

        assert_eq!(
            invite_account(&users, &inv, "bob@example.io", &HashSet::new(), NOW)
                .await
                .unwrap(),
            Invited::EmailTaken
        );
    }

    #[tokio::test]
    async fn a_failed_mail_keeps_the_account() {
        let users = users();
        let inv = invitation(Arc::new(FailingMailer));
        let Invited::Created { user, sent } =
            invite_account(&users, &inv, "bob@example.io", &HashSet::new(), NOW)
                .await
                .unwrap()
        else {
            panic!("création");
        };
        assert!(!sent);
        assert!(users.find_by_id(&user.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn operator_invitation_validates_and_audits() {
        let users = users();
        let audit = InMemoryAuditSink::new();
        let inv = invitation(Arc::new(MemoryMailer::new()));
        let Invited::Created { user, .. } = invite_by_operator(
            &users,
            &audit,
            &inv,
            " Root@Example.io ",
            Some("admin"),
            NOW * 1_000,
        )
        .await
        .unwrap() else {
            panic!("création");
        };
        assert_eq!(user.email, "root@example.io");
        assert_eq!(
            users.summary(&user.id).await.unwrap().unwrap().permissions,
            Role::super_admin().permissions
        );
        let events = audit.list().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor, "cli");
        assert_eq!(events[0].action, "admin.user_invite");
        assert_eq!(events[0].resource.as_deref(), Some(user.id.as_str()));

        assert!(
            invite_by_operator(&users, &audit, &inv, "x@example.io", Some("owner"), 0)
                .await
                .unwrap_err()
                .contains("rôle inconnu")
        );
        assert!(
            invite_by_operator(&users, &audit, &inv, "pas-un-email", None, 0)
                .await
                .is_err()
        );
        assert_eq!(
            invite_by_operator(&users, &audit, &inv, "root@example.io", None, 0)
                .await
                .unwrap(),
            Invited::EmailTaken
        );
        assert_eq!(audit.list().await.unwrap().len(), 1, "seule la création");
    }
}

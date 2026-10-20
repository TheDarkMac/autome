//! Envoi d'emails transactionnels (#204 — mot de passe oublié).
//!
//! Le contrat [`Mailer`] envoie un message texte à une adresse. Trois impls :
//!
//! | Impl              | Usage                                                         |
//! |-------------------|---------------------------------------------------------------|
//! | [`SmtpMailer`]    | Production : SMTP via `lettre` (TLS rustls), `AUTOME_SMTP_URL` |
//! | [`LogMailer`]     | Profil `dev` sans SMTP : le message est **journalisé**        |
//! | [`MemoryMailer`]  | Tests : les messages sont conservés en mémoire                |
//!
//! [`MailSettings::from_env`] choisit l'impl : SMTP s'il est configuré
//! (`AUTOME_MAIL_FROM` alors obligatoire), sinon journal en `dev` et **aucun
//! envoi** en `prod` (les fonctions qui en dépendent répondent 503). Une
//! configuration invalide refuse le démarrage.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use autome_core::Profile;
use lettre::message::{header::ContentType, Mailbox};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// Délai maximal d'une session SMTP (connexion, TLS, envoi).
pub const SMTP_TIMEOUT: Duration = Duration::from_secs(10);
/// URL publique du frontend en `dev` quand `AUTOME_PUBLIC_URL` est absente
/// (serveur Vite).
pub const DEV_PUBLIC_URL: &str = "http://localhost:5173";

/// Message à envoyer (texte brut).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMail {
    /// Destinataire (adresse seule).
    pub to: String,
    /// Objet.
    pub subject: String,
    /// Corps en texte brut.
    pub body: String,
}

/// Envoi d'un email. L'erreur est un message destiné aux journaux : elle ne
/// doit pas contenir le corps du message (il peut porter un lien secret).
#[async_trait]
pub trait Mailer: Send + Sync {
    /// Envoie `mail`.
    async fn send(&self, mail: &OutgoingMail) -> Result<(), String>;
}

/// Envoi SMTP (`lettre`, runtime tokio, TLS rustls).
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl std::fmt::Debug for SmtpMailer {
    // L'URL SMTP peut contenir le mot de passe du relais : jamais affichée.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpMailer")
            .field("from", &self.from.to_string())
            .finish_non_exhaustive()
    }
}

impl SmtpMailer {
    /// Construit le transport depuis une URL `lettre` : `smtps://user:pass@host`
    /// (TLS implicite, port 465), `smtp://host:587?tls=required` (STARTTLS
    /// exigé) ou `smtp://host:25` (en clair, réseau de confiance seulement).
    /// Aucune connexion n'est ouverte ici.
    pub fn new(url: &str, from: &str) -> Result<Self, String> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            // Le message d'erreur de `lettre` peut reprendre l'URL (secret).
            .map_err(|_| "AUTOME_SMTP_URL : URL SMTP invalide".to_string())?
            .timeout(Some(SMTP_TIMEOUT))
            .build();
        let from = from
            .parse::<Mailbox>()
            .map_err(|e| format!("AUTOME_MAIL_FROM : adresse invalide ({e})"))?;
        Ok(Self { transport, from })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, mail: &OutgoingMail) -> Result<(), String> {
        let to = mail
            .to
            .parse::<Mailbox>()
            .map_err(|e| format!("destinataire invalide : {e}"))?;
        let message = Message::builder()
            // Sans `Message-ID`, des relais et filtres anti-spam pénalisent
            // le message ; domaine de l'expéditeur plutôt que nom d'hôte local.
            .message_id(Some(message_id(self.from.email.domain())))
            .from(self.from.clone())
            .to(to)
            .subject(mail.subject.clone())
            .header(ContentType::TEXT_PLAIN)
            .body(mail.body.clone())
            .map_err(|e| format!("message invalide : {e}"))?;
        self.transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|e| format!("envoi SMTP : {e}"))
    }
}

/// `Message-ID` aléatoire (128 bits) sous le domaine `domain`.
fn message_id(domain: &str) -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    format!("<{}@{domain}>", hex::encode(bytes))
}

/// Profil `dev` sans SMTP : le message (lien compris) est écrit dans les
/// journaux. **Jamais en production** : le lien est un secret.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogMailer;

#[async_trait]
impl Mailer for LogMailer {
    async fn send(&self, mail: &OutgoingMail) -> Result<(), String> {
        tracing::warn!(
            to = %mail.to,
            subject = %mail.subject,
            body = %mail.body,
            "email non envoyé (pas de SMTP, profil dev) : contenu journalisé"
        );
        Ok(())
    }
}

/// Mailer de test : conserve les messages envoyés.
#[derive(Debug, Default, Clone)]
pub struct MemoryMailer {
    sent: Arc<Mutex<Vec<OutgoingMail>>>,
}

impl MemoryMailer {
    /// Mailer vide.
    pub fn new() -> Self {
        Self::default()
    }

    /// Messages envoyés jusqu'ici, dans l'ordre.
    pub fn sent(&self) -> Vec<OutgoingMail> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl Mailer for MemoryMailer {
    async fn send(&self, mail: &OutgoingMail) -> Result<(), String> {
        self.sent
            .lock()
            .map_err(|_| "verrou empoisonné".to_string())?
            .push(mail.clone());
        Ok(())
    }
}

/// Configuration de l'envoi d'emails et de l'URL publique du frontend.
#[derive(Clone)]
pub struct MailSettings {
    /// Transport retenu.
    pub mailer: Arc<dyn Mailer>,
    /// URL publique du frontend, sans `/` final (base des liens envoyés).
    pub public_url: String,
}

impl std::fmt::Debug for MailSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailSettings")
            .field("public_url", &self.public_url)
            .finish_non_exhaustive()
    }
}

impl MailSettings {
    /// Depuis l'environnement : `AUTOME_SMTP_URL`, `AUTOME_MAIL_FROM`,
    /// `AUTOME_PUBLIC_URL`. `Ok(None)` : aucun envoi possible (`prod` sans
    /// SMTP).
    pub fn from_env(profile: Profile) -> Result<Option<Self>, String> {
        Self::from_source(profile, |name| std::env::var(name).ok())
    }

    /// Comme [`from_env`](Self::from_env), variables lues via `get` (tests).
    pub fn from_source(
        profile: Profile,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let var = |name: &str| {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let public_url = var("AUTOME_PUBLIC_URL")
            .map(|u| parse_public_url(&u))
            .transpose()?;

        let Some(smtp_url) = var("AUTOME_SMTP_URL") else {
            return Ok(match profile {
                Profile::Dev => Some(Self {
                    mailer: Arc::new(LogMailer),
                    public_url: public_url.unwrap_or_else(|| DEV_PUBLIC_URL.to_string()),
                }),
                Profile::Prod => None,
            });
        };
        let from = var("AUTOME_MAIL_FROM")
            .ok_or("AUTOME_MAIL_FROM est obligatoire quand AUTOME_SMTP_URL est défini")?;
        // Le lien ne doit jamais être construit depuis l'en-tête `Host` de la
        // requête (empoisonnement du lien) : l'URL publique est explicite.
        let public_url = match (public_url, profile) {
            (Some(url), _) => url,
            (None, Profile::Dev) => DEV_PUBLIC_URL.to_string(),
            (None, Profile::Prod) => {
                return Err(
                    "AUTOME_PUBLIC_URL est obligatoire quand AUTOME_SMTP_URL est défini".into(),
                )
            }
        };
        Ok(Some(Self {
            mailer: Arc::new(SmtpMailer::new(&smtp_url, &from)?),
            public_url,
        }))
    }
}

/// Valide une URL publique `http(s)://hôte[...]` et retire le `/` final.
fn parse_public_url(raw: &str) -> Result<String, String> {
    let invalid = || format!("AUTOME_PUBLIC_URL : URL http(s) attendue, obtenu `{raw}`");
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .ok_or_else(invalid)?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || raw.contains(['?', '#']) || raw.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    Ok(raw.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn message_ids_are_unique_under_the_sender_domain() {
        let (a, b) = (message_id("example.io"), message_id("example.io"));
        assert!(a.starts_with('<') && a.ends_with("@example.io>"));
        assert_eq!(a.len(), "<@example.io>".len() + 32);
        assert_ne!(a, b);
    }

    #[test]
    fn public_url_is_validated_and_trimmed() {
        assert_eq!(
            parse_public_url("https://app.example.io/").unwrap(),
            "https://app.example.io"
        );
        assert_eq!(
            parse_public_url("http://localhost:5173/autome").unwrap(),
            "http://localhost:5173/autome"
        );
        for bad in [
            "app.example.io",
            "ftp://app.example.io",
            "https://",
            "https://app.example.io/?x=1",
            "https://app.example.io/#a",
            "https://app example.io",
        ] {
            assert!(parse_public_url(bad).is_err(), "devrait refuser {bad:?}");
        }
    }

    #[test]
    fn dev_without_smtp_logs_with_default_public_url() {
        let s = MailSettings::from_source(Profile::Dev, env(&[]))
            .unwrap()
            .expect("journal en dev");
        assert_eq!(s.public_url, DEV_PUBLIC_URL);
    }

    #[test]
    fn prod_without_smtp_disables_mail() {
        let s = MailSettings::from_source(
            Profile::Prod,
            env(&[("AUTOME_PUBLIC_URL", "https://app.example.io")]),
        )
        .unwrap();
        assert!(s.is_none());
    }

    #[test]
    fn smtp_requires_sender_and_public_url_in_prod() {
        let missing_from = MailSettings::from_source(
            Profile::Prod,
            env(&[
                ("AUTOME_SMTP_URL", "smtps://u:p@smtp.example.io"),
                ("AUTOME_PUBLIC_URL", "https://app.example.io"),
            ]),
        );
        assert!(missing_from.unwrap_err().contains("AUTOME_MAIL_FROM"));

        let missing_url = MailSettings::from_source(
            Profile::Prod,
            env(&[
                ("AUTOME_SMTP_URL", "smtps://u:p@smtp.example.io"),
                ("AUTOME_MAIL_FROM", "Autome <no-reply@example.io>"),
            ]),
        );
        assert!(missing_url.unwrap_err().contains("AUTOME_PUBLIC_URL"));
    }

    #[test]
    fn smtp_settings_build_without_connecting() {
        let s = MailSettings::from_source(
            Profile::Prod,
            env(&[
                ("AUTOME_SMTP_URL", "smtps://u:secret@smtp.example.io"),
                ("AUTOME_MAIL_FROM", "Autome <no-reply@example.io>"),
                ("AUTOME_PUBLIC_URL", "https://app.example.io/"),
            ]),
        )
        .unwrap()
        .expect("SMTP configuré");
        assert_eq!(s.public_url, "https://app.example.io");
        assert!(!format!("{s:?}").contains("secret"));
    }

    #[test]
    fn invalid_smtp_settings_are_rejected_without_leaking_url() {
        let err = MailSettings::from_source(
            Profile::Prod,
            env(&[
                ("AUTOME_SMTP_URL", "imap://u:secret@smtp.example.io"),
                ("AUTOME_MAIL_FROM", "no-reply@example.io"),
                ("AUTOME_PUBLIC_URL", "https://app.example.io"),
            ]),
        )
        .unwrap_err();
        assert!(err.contains("AUTOME_SMTP_URL"));
        assert!(!err.contains("secret"));

        let err = MailSettings::from_source(
            Profile::Dev,
            env(&[
                ("AUTOME_SMTP_URL", "smtp://localhost:1025"),
                ("AUTOME_MAIL_FROM", "pas une adresse"),
            ]),
        )
        .unwrap_err();
        assert!(err.contains("AUTOME_MAIL_FROM"));
    }

    #[tokio::test]
    async fn memory_mailer_records_messages() {
        let mailer = MemoryMailer::new();
        let mail = OutgoingMail {
            to: "alice@example.io".into(),
            subject: "s".into(),
            body: "b".into(),
        };
        mailer.send(&mail).await.unwrap();
        assert_eq!(mailer.sent(), vec![mail]);
    }
}

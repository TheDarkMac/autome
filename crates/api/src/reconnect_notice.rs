//! Email « connecteur à reconnecter » (#265).
//!
//! Le worker marque une credential invalide quand le provider refuse son
//! refresh token (#211) ; jusqu'à la reconnexion, les workflows qui
//! l'utilisent échouent — y compris ceux qui partent seuls (planifiés,
//! polling, webhooks). L'onglet Connecteurs et l'en-tête le signalent (#213,
//! #265), mais seulement à qui ouvre l'application : [`ReconnectNotifier`]
//! prévient aussi le propriétaire par email.
//!
//! L'API, qui a le mailer et les comptes, réclame périodiquement les
//! invalidations non notifiées ([`ReconnectNotices`] : chacune une fois, même
//! avec plusieurs répliques) et envoie un message par credential. Envoi **au
//! plus une fois** : un échec SMTP est journalisé, pas retenté (l'en-tête
//! reste là). Aucun envoi vers un compte suspendu ou dont l'adresse n'est pas
//! vérifiée. Le message ne reprend pas le motif brut du provider.

use std::sync::Arc;
use std::time::Duration;

use autome_connectors::oauth::{ReconnectNotice, ReconnectNotices};
use autome_core::{Error, Result, UserStore};

use crate::mail::{MailSettings, OutgoingMail};

/// Période entre deux tours de notification.
pub const RECONNECT_NOTICE_INTERVAL: Duration = Duration::from_secs(60);
/// Invalidations réclamées par lot.
pub const RECONNECT_NOTICE_BATCH: usize = 50;
/// Fragment de l'URL publique qui ouvre l'onglet Connecteurs.
pub const CONNECTORS_FRAGMENT: &str = "#connecteurs";

/// Bilan d'un tour de notification.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NoticeReport {
    /// Emails envoyés.
    pub sent: usize,
    /// Invalidations sans destinataire utilisable (compte absent, suspendu
    /// ou adresse non vérifiée).
    pub skipped: usize,
    /// Envois refusés par le transport (non retentés).
    pub failed: usize,
}

/// Envoi des emails « à reconnecter ».
pub struct ReconnectNotifier {
    notices: Arc<dyn ReconnectNotices + Send + Sync>,
    users: Arc<dyn UserStore + Send + Sync>,
    mail: MailSettings,
}

impl ReconnectNotifier {
    /// Notifications réclamées dans `notices`, destinataires lus dans `users`.
    pub fn new(
        notices: Arc<dyn ReconnectNotices + Send + Sync>,
        users: Arc<dyn UserStore + Send + Sync>,
        mail: MailSettings,
    ) -> Self {
        Self {
            notices,
            users,
            mail,
        }
    }

    /// Lien vers l'onglet Connecteurs.
    pub fn link(&self) -> String {
        format!("{}/{CONNECTORS_FRAGMENT}", self.mail.public_url)
    }

    /// Email pour `to` : la credential `provider` est à reconnecter.
    pub fn mail_for(&self, to: &str, provider: &str) -> OutgoingMail {
        OutgoingMail {
            to: to.to_string(),
            subject: format!("Autome : reconnectez votre compte {provider}"),
            body: format!(
                "Bonjour,\n\n\
                 Le service {provider} a refusé l'accès qu'il avait accordé à Autome \
                 (autorisation révoquée ou expirée). Les workflows qui utilisent ce \
                 connecteur échouent jusqu'à ce que vous le reconnectiez :\n\n\
                 {}\n\n\
                 Onglet Connecteurs, bouton « Reconnecter ».\n",
                self.link()
            ),
        }
    }

    /// Réclame et envoie les notifications en attente, lot après lot.
    pub async fn run_once(&self) -> Result<NoticeReport> {
        let mut report = NoticeReport::default();
        loop {
            let batch = self.claim().await?;
            let full = batch.len() == RECONNECT_NOTICE_BATCH;
            for notice in batch {
                self.notify(&notice, &mut report).await;
            }
            if !full {
                return Ok(report);
            }
        }
    }

    /// Lot suivant. Le store est synchrone (pont `block_in_place` en
    /// PostgreSQL) : réclamé hors des threads de l'exécuteur.
    async fn claim(&self) -> Result<Vec<ReconnectNotice>> {
        let notices = Arc::clone(&self.notices);
        tokio::task::spawn_blocking(move || notices.claim_reconnect_notices(RECONNECT_NOTICE_BATCH))
            .await
            .map_err(|e| Error::Invalid(format!("réclamation des notifications : {e}")))?
    }

    async fn notify(&self, notice: &ReconnectNotice, report: &mut NoticeReport) {
        let provider = notice.provider_id.as_str();
        let user = match self.users.find_by_id(&notice.user_id).await {
            Ok(Some(user)) if !user.disabled && user.email_verified => user,
            Ok(_) => {
                tracing::debug!(
                    user_id = %notice.user_id,
                    provider,
                    "credential à reconnecter : compte absent, suspendu ou non vérifié, pas d'email"
                );
                report.skipped += 1;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    user_id = %notice.user_id,
                    provider,
                    error = %e,
                    "credential à reconnecter : compte illisible, email non envoyé"
                );
                report.failed += 1;
                return;
            }
        };
        match self
            .mail
            .mailer
            .send(&self.mail_for(&user.email, provider))
            .await
        {
            Ok(()) => {
                tracing::info!(user_id = %user.id, provider, "email « à reconnecter » envoyé");
                report.sent += 1;
            }
            Err(e) => {
                tracing::warn!(
                    user_id = %user.id,
                    provider,
                    error = %e,
                    "email « à reconnecter » non envoyé"
                );
                report.failed += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use autome_connectors::oauth::{
        ConnectorTokenStore, CredentialInvalidation, CredentialUpdate, InMemoryConnectorTokenStore,
        TokenSet,
    };
    use autome_core::{
        CreateUser, EnvelopeCipher, InMemoryKeyProvider, InMemoryPermissionStore, InMemoryUserStore,
    };

    use crate::mail::{Mailer, MemoryMailer};

    type Tokens = InMemoryConnectorTokenStore<InMemoryKeyProvider>;

    struct Fixture {
        tokens: Arc<Tokens>,
        users: Arc<InMemoryUserStore>,
        mailer: MemoryMailer,
        notifier: ReconnectNotifier,
    }

    fn fixture_with(mailer: Arc<dyn Mailer>, sent: MemoryMailer) -> Fixture {
        let tokens = Arc::new(Tokens::new(EnvelopeCipher::new(
            InMemoryKeyProvider::with_key("k1", [1u8; 32]),
        )));
        let users = Arc::new(InMemoryUserStore::new(Arc::new(
            InMemoryPermissionStore::new(),
        )));
        let notifier = ReconnectNotifier::new(
            tokens.clone(),
            users.clone(),
            MailSettings {
                mailer,
                public_url: "https://app.example.io".into(),
            },
        );
        Fixture {
            tokens,
            users,
            mailer: sent,
            notifier,
        }
    }

    fn fixture() -> Fixture {
        let mailer = MemoryMailer::new();
        fixture_with(Arc::new(mailer.clone()), mailer)
    }

    async fn user(users: &InMemoryUserStore, email: &str, verified: bool) -> String {
        match users
            .create(email, "hash", &HashSet::new(), verified)
            .await
            .unwrap()
        {
            CreateUser::Created(user) => user.id,
            CreateUser::EmailTaken => panic!("email déjà pris"),
        }
    }

    fn revoked(tokens: &Tokens, user_id: &str, provider: &str, at_ms: i64) {
        let set = TokenSet {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at_ms: Some(1),
            token_type: "Bearer".into(),
            scope: None,
        };
        tokens.store(user_id, provider, &set).unwrap();
        tokens
            .update_locked(user_id, provider, &mut |_| {
                CredentialUpdate::Invalidate(CredentialInvalidation {
                    at_ms,
                    reason: "invalid_grant: token secret-xyz révoqué".into(),
                })
            })
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_owner_is_emailed_once_per_invalidation() {
        let f = fixture();
        let alice = user(&f.users, "alice@example.io", true).await;
        revoked(&f.tokens, &alice, "slack", 100);

        let report = f.notifier.run_once().await.unwrap();
        assert_eq!(
            report,
            NoticeReport {
                sent: 1,
                ..NoticeReport::default()
            }
        );
        let sent = f.mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "alice@example.io");
        assert_eq!(sent[0].subject, "Autome : reconnectez votre compte slack");
        assert!(
            sent[0].body.contains("https://app.example.io/#connecteurs"),
            "{}",
            sent[0].body
        );
        assert!(
            !sent[0].body.contains("invalid_grant") && !sent[0].body.contains("secret-xyz"),
            "motif brut du provider jamais recopié"
        );

        // Tour suivant : rien de neuf.
        assert_eq!(
            f.notifier.run_once().await.unwrap(),
            NoticeReport::default()
        );
        assert_eq!(f.mailer.sent().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn suspended_unverified_or_unknown_owners_are_not_emailed() {
        let f = fixture();
        let bob = user(&f.users, "bob@example.io", true).await;
        f.users.set_disabled(&bob, true).await.unwrap();
        let carol = user(&f.users, "carol@example.io", false).await;
        revoked(&f.tokens, &bob, "github", 1);
        revoked(&f.tokens, &carol, "notion", 2);
        revoked(&f.tokens, "ghost", "gmail", 3);

        let report = f.notifier.run_once().await.unwrap();
        assert_eq!(
            report,
            NoticeReport {
                skipped: 3,
                ..NoticeReport::default()
            }
        );
        assert!(f.mailer.sent().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_batch_is_drained_in_one_round() {
        let f = fixture();
        let dave = user(&f.users, "dave@example.io", true).await;
        let total = RECONNECT_NOTICE_BATCH + 3;
        for i in 0..total {
            revoked(&f.tokens, &dave, &format!("provider-{i:03}"), i as i64);
        }
        assert_eq!(f.notifier.run_once().await.unwrap().sent, total);
        assert_eq!(f.mailer.sent().len(), total);
    }

    /// Transport en panne.
    struct DownMailer;

    #[async_trait::async_trait]
    impl Mailer for DownMailer {
        async fn send(&self, _mail: &OutgoingMail) -> std::result::Result<(), String> {
            Err("smtp en panne".into())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_send_is_reported_and_not_retried() {
        let f = fixture_with(Arc::new(DownMailer), MemoryMailer::new());
        let erin = user(&f.users, "erin@example.io", true).await;
        revoked(&f.tokens, &erin, "slack", 1);

        assert_eq!(
            f.notifier.run_once().await.unwrap(),
            NoticeReport {
                failed: 1,
                ..NoticeReport::default()
            }
        );
        assert_eq!(
            f.notifier.run_once().await.unwrap(),
            NoticeReport::default()
        );
    }
}

//! Effacement d'un compte côté API (RGPD, droit à l'effacement, #229).
//!
//! [`erase_account`] est le chemin commun de `DELETE /auth/me`
//! ([`crate::accounts`]) et de `DELETE /admin/users/{id}` ([`crate::admin`]) :
//!
//! 1. **toutes les sessions** sont fermées d'abord (version de session,
//!    #186) : une requête qui arrive pendant l'effacement est refusée, et un
//!    effacement qui échoue laisse au pire un compte déconnecté, intact ;
//! 2. l'[`AccountEraser`] efface le compte et ses données en une opération
//!    (une transaction en PostgreSQL) et pseudonymise l'audit ;
//! 3. les compteurs anti-bruteforce du compte (email, mot de passe,
//!    réinitialisation) sont remis à zéro — de toute façon hachés, ils
//!    expireraient avec leur fenêtre.
//!
//! Gardés après l'effacement : la ligne `session_versions` (un id aléatoire et
//! un entier ; la supprimer revaliderait les anciens jetons) et les `jti`
//! révoqués (sans lien avec le compte).
//!
//! [`InMemoryAccountEraser`] est l'effaceur du mode `memory` et des tests.

use std::sync::Arc;

use async_trait::async_trait;
use autome_connectors::oauth::ConnectorTokenStore;
use autome_core::{
    email_throttle_key, erasure_pseudonym, AccountEraser, ErasureReport, InMemoryAuditSink,
    InMemoryRunStateStore, InMemoryUserStore, LoginThrottleStore, Pagination, TriggerScheduleStore,
    UserRecord, WebhookEndpointStore, WorkflowRepository,
};

use crate::auth::AuthState;

/// Effaceur et compteurs anti-bruteforce, partagés par les deux routes.
#[derive(Clone)]
pub struct Erasure {
    /// Effacement des données.
    pub eraser: Arc<dyn AccountEraser>,
    /// Compteurs anti-bruteforce (remis à zéro après effacement).
    pub throttle: Arc<dyn LoginThrottleStore>,
}

impl Erasure {
    /// Construit l'état.
    pub fn new(eraser: Arc<dyn AccountEraser>, throttle: Arc<dyn LoginThrottleStore>) -> Self {
        Self { eraser, throttle }
    }
}

/// Résultat d'un effacement réussi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Erased {
    /// Pseudonyme qui remplace le compte dans l'audit.
    pub pseudonym: String,
    /// Ce qui a été effacé.
    pub report: ErasureReport,
}

/// Ferme les sessions de `account`, l'efface et remet ses compteurs à zéro.
/// `None` si le compte n'existait plus (effacement concurrent).
pub async fn erase_account(
    auth: &AuthState,
    erasure: &Erasure,
    account: &UserRecord,
) -> autome_core::Result<Option<Erased>> {
    auth.revocation.bump_session_version(&account.id).await?;
    let pseudonym = erasure_pseudonym();
    let Some(report) = erasure.eraser.erase(&account.id, &pseudonym).await? else {
        return Ok(None);
    };
    for key in [
        email_throttle_key(&account.email),
        format!("password:{}", account.id),
        format!("reset:{}", account.email),
    ] {
        if let Err(e) = erasure.throttle.reset(&key).await {
            tracing::warn!(error = %e, "effacement : remise à zéro d'un compteur impossible");
        }
    }
    tracing::info!(
        workflows = report.workflows,
        runs = report.runs,
        connector_tokens = report.connector_tokens,
        audit_events = report.audit_events,
        "compte effacé"
    );
    Ok(Some(Erased { pseudonym, report }))
}

/// Effaceur du mode `memory` (dev, tests) : mêmes effets que l'impl
/// PostgreSQL, étape par étape (pas de transaction entre stores en mémoire).
pub struct InMemoryAccountEraser {
    users: Arc<InMemoryUserStore>,
    workflows: Arc<dyn WorkflowRepository + Send + Sync>,
    runs: Arc<InMemoryRunStateStore>,
    tokens: Arc<dyn ConnectorTokenStore + Send + Sync>,
    webhooks: Arc<dyn WebhookEndpointStore>,
    schedules: Arc<dyn TriggerScheduleStore>,
    audit: Option<Arc<InMemoryAuditSink>>,
}

impl InMemoryAccountEraser {
    /// Construit l'effaceur sur les stores en mémoire de l'instance ; `audit`
    /// `None` si l'audit n'est pas conservé (`NoopAuditSink`).
    pub fn new(
        users: Arc<InMemoryUserStore>,
        workflows: Arc<dyn WorkflowRepository + Send + Sync>,
        runs: Arc<InMemoryRunStateStore>,
        tokens: Arc<dyn ConnectorTokenStore + Send + Sync>,
        webhooks: Arc<dyn WebhookEndpointStore>,
        schedules: Arc<dyn TriggerScheduleStore>,
        audit: Option<Arc<InMemoryAuditSink>>,
    ) -> Self {
        Self {
            users,
            workflows,
            runs,
            tokens,
            webhooks,
            schedules,
            audit,
        }
    }
}

#[async_trait]
impl AccountEraser for InMemoryAccountEraser {
    async fn erase(
        &self,
        user_id: &str,
        pseudonym: &str,
    ) -> autome_core::Result<Option<ErasureReport>> {
        use autome_core::UserStore;
        let Some(account) = self.users.find_by_id(user_id).await? else {
            return Ok(None);
        };
        let mut report = ErasureReport::default();

        // Une page couvre tout l'espace : `delete` le vide au fil de l'eau.
        loop {
            let page = self
                .workflows
                .list(user_id, Pagination::default_paging())
                .await?;
            if page.items.is_empty() {
                break;
            }
            for wf in page.items {
                self.webhooks.retain(user_id, &wf.id.0, &[]).await?;
                self.schedules.sync(user_id, &wf.id.0, &[]).await?;
                if self.workflows.delete(user_id, &wf.id).await? {
                    report.workflows += 1;
                }
            }
        }
        report.runs = self.runs.remove_owner(user_id)?;
        for provider in self.tokens.list_by_user(user_id)? {
            self.tokens.delete(user_id, &provider)?;
            report.connector_tokens += 1;
        }
        if let Some(audit) = &self.audit {
            report.audit_events = audit.pseudonymize(&[user_id, &account.email], pseudonym)?;
        }
        self.users.remove(user_id)?;
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_connectors::oauth::{InMemoryConnectorTokenStore, TokenSet};
    use autome_core::{
        AuditEvent, AuditOutcome, AuditSink, CreateUser, EndpointSecret, EnvelopeCipher,
        InMemoryKeyProvider, InMemoryPermissionStore, InMemoryTriggerScheduleStore,
        InMemoryWebhookEndpointStore, InMemoryWorkflowRepository, Node, NodeId, NodeKind,
        PermissionStore, RunCheckpoint, RunStateStore, ScheduledTrigger, UserStore, WorkflowGraph,
        WorkflowId,
    };
    use serde_json::{json, Value};
    use std::collections::HashSet;

    struct Stores {
        perms: Arc<InMemoryPermissionStore>,
        users: Arc<InMemoryUserStore>,
        workflows: Arc<InMemoryWorkflowRepository>,
        runs: Arc<InMemoryRunStateStore>,
        tokens: Arc<InMemoryConnectorTokenStore<InMemoryKeyProvider>>,
        webhooks: Arc<InMemoryWebhookEndpointStore>,
        schedules: Arc<InMemoryTriggerScheduleStore>,
        audit: Arc<InMemoryAuditSink>,
    }

    impl Stores {
        fn new() -> Self {
            let perms = Arc::new(InMemoryPermissionStore::new());
            Self {
                users: Arc::new(InMemoryUserStore::new(perms.clone())),
                perms,
                workflows: Arc::new(InMemoryWorkflowRepository::new()),
                runs: Arc::new(InMemoryRunStateStore::new()),
                tokens: Arc::new(InMemoryConnectorTokenStore::new(EnvelopeCipher::new(
                    InMemoryKeyProvider::with_key("v1", [3u8; 32]),
                ))),
                webhooks: Arc::new(InMemoryWebhookEndpointStore::new()),
                schedules: Arc::new(InMemoryTriggerScheduleStore::new()),
                audit: Arc::new(InMemoryAuditSink::new()),
            }
        }

        fn eraser(&self) -> InMemoryAccountEraser {
            InMemoryAccountEraser::new(
                self.users.clone(),
                self.workflows.clone(),
                self.runs.clone(),
                self.tokens.clone(),
                self.webhooks.clone(),
                self.schedules.clone(),
                Some(self.audit.clone()),
            )
        }

        async fn seed(&self, email: &str, workflows: usize) -> String {
            let perms: HashSet<String> = ["workflows.read".to_string()].into();
            let CreateUser::Created(user) = self.users.create(email, "h", &perms).await.unwrap()
            else {
                panic!("compte");
            };
            let id = user.id;
            for i in 0..workflows {
                let wf_id = format!("wf-{i}");
                self.workflows.create(&id, workflow(&wf_id)).await.unwrap();
                let secret = EndpointSecret {
                    salt: vec![1; 32],
                    token_hash: format!("{email}{i}").into_bytes(),
                };
                self.webhooks
                    .ensure(&id, &wf_id, "hook", secret)
                    .await
                    .unwrap();
                self.schedules
                    .sync(
                        &id,
                        &wf_id,
                        &[ScheduledTrigger {
                            node_id: "tick".into(),
                            trigger_type: "schedule".into(),
                            params: json!({ "interval_seconds": 60 }),
                        }],
                    )
                    .await
                    .unwrap();
            }
            self.runs
                .checkpoint(
                    &RunCheckpoint::queued(format!("run-{email}"), "wf-0", 1, "manual", 1)
                        .with_owner(Some(id.clone())),
                )
                .await
                .unwrap();
            let set = TokenSet {
                access_token: "t".into(),
                refresh_token: None,
                expires_at_ms: None,
                token_type: "Bearer".into(),
                scope: None,
            };
            self.tokens.store(&id, "slack", &set).unwrap();
            self.tokens.store(&id, "github", &set).unwrap();
            for event in [
                AuditEvent::new(1, email, "auth.login", AuditOutcome::Failure),
                AuditEvent::new(2, id.as_str(), "auth.login", AuditOutcome::Success),
            ] {
                self.audit.record(event).await.unwrap();
            }
            id
        }
    }

    fn workflow(id: &str) -> WorkflowGraph {
        let trigger = |id: &str, t: &str| Node {
            position: None,
            id: NodeId::new(id),
            name: id.into(),
            kind: NodeKind::Trigger {
                trigger_type: t.into(),
            },
            params: Value::Null,
        };
        WorkflowGraph {
            id: WorkflowId::new(id),
            name: id.into(),
            nodes: vec![trigger("hook", "webhook"), trigger("tick", "schedule")],
            edges: vec![],
        }
    }

    #[tokio::test]
    async fn erase_removes_the_account_data_and_pseudonymizes_the_audit() {
        let s = Stores::new();
        // Plus d'une page de workflows.
        let pages = Pagination::default_paging().limit + 3;
        let alice = s.seed("alice@example.io", pages).await;
        let bob = s.seed("bob@example.io", 1).await;

        let report = s.eraser().erase(&alice, "erased:aa").await.unwrap();
        assert_eq!(
            report,
            Some(ErasureReport {
                workflows: pages,
                runs: 1,
                connector_tokens: 2,
                audit_events: 2,
            })
        );
        assert_eq!(s.users.find_by_id(&alice).await.unwrap(), None);
        assert!(s.perms.permissions_for(&alice).await.unwrap().is_empty());
        assert_eq!(
            s.workflows
                .list(&alice, Pagination::default_paging())
                .await
                .unwrap()
                .total,
            0
        );
        assert!(s.runs.load("run-alice@example.io").await.unwrap().is_none());
        assert!(s.tokens.list_by_user(&alice).unwrap().is_empty());
        assert!(s
            .schedules
            .due(i64::MAX, 1_000)
            .await
            .unwrap()
            .iter()
            .all(|slot| slot.owner_id != alice));
        assert!(s
            .webhooks
            .resolve(b"alice@example.io0")
            .await
            .unwrap()
            .is_none());
        let audit = s.audit.list().await.unwrap();
        assert!(audit
            .iter()
            .all(|e| e.actor != alice && e.actor != "alice@example.io"));
        assert_eq!(audit.iter().filter(|e| e.actor == "erased:aa").count(), 2);

        // Bob intact.
        assert!(s.users.find_by_id(&bob).await.unwrap().is_some());
        assert_eq!(s.tokens.list_by_user(&bob).unwrap().len(), 2);
        assert!(s.runs.load("run-bob@example.io").await.unwrap().is_some());
        assert!(s
            .webhooks
            .resolve(b"bob@example.io0")
            .await
            .unwrap()
            .is_some());
        assert!(audit.iter().any(|e| e.actor == bob));

        assert_eq!(s.eraser().erase(&alice, "erased:bb").await.unwrap(), None);
    }
}

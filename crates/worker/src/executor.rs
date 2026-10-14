//! Executor de **production** : le [`Handler`] que le binaire `autome-worker`
//! branche sur la file réelle (dette #107, constat 2).
//!
//! Il promeut en production la logique éprouvée par le test e2e (F9.4) :
//!
//! ```text
//! message {run_id, workflow_id}  →  WorkflowRepository.get
//!                                →  RunStateStore (Queued → Running)
//!                                →  Engine.run (DAG + node runner)
//!                                →  RunStateStore (sorties + Succeeded/Failed)
//! ```
//!
//! Le **runner de nodes** traduit chaque node en effet concret :
//! - `Trigger` : pass-through (le déclencheur a déjà eu lieu côté API) ;
//! - `Action { action_type: "http_request" }` : [`HttpAction`] via un
//!   [`ReqwestHttpClient`] **partagé** (un seul client bloquant réutilisé),
//!   filtré anti-SSRF : destinations publiques seulement, sauf hôtes de
//!   l'allowlist opérateur `AUTOME_EGRESS_ALLOWLIST` ;
//! - `Action { slack.* | gmail.* | notion.* | github.* }` : connecteur, avec le
//!   token **du propriétaire du run** (`RunCheckpoint.owner_id`) résolu par le
//!   [`CredentialResolver`] ;
//! - tout autre `action_type` : erreur.
//!
//! Le moteur est **synchrone** et le transport HTTP est **bloquant**
//! (`reqwest::blocking`) : exécuter `Engine::run` directement sur un thread
//! worker de tokio paniquerait (« cannot start a runtime from within a
//! runtime »). On déporte donc toute l'exécution moteur dans
//! [`tokio::task::spawn_blocking`].

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use autome_connectors::{EgressPolicy, HttpAction, HttpClient, ReqwestHttpClient};
use autome_core::{
    Action, Delivery, Node, NodeKind, RunStateStore, RunStatus, WorkflowId, WorkflowRepository,
};
use autome_engine::{Dag, Engine, RunContext, StepStatus};

use crate::dispatch::{dispatch_action, CredentialResolver};
use crate::{Handler, HandlerVerdict};

/// Handler de production : exécute un run de bout en bout et persiste son état.
///
/// Construit avec des `Arc<dyn …>` (repo/store sélectionnés par
/// `AUTOME_PERSISTENCE`), un transport HTTP clonable partagé entre toutes les
/// actions (par défaut [`ReqwestHttpClient`] filtré anti-SSRF), et un
/// [`CredentialResolver`] qui injecte le token du propriétaire du run dans les
/// connecteurs. Le résolveur est **obligatoire** : aucun défaut implicite vers
/// un token global.
pub struct RunExecutor<C = ReqwestHttpClient> {
    repo: Arc<dyn WorkflowRepository + Send + Sync>,
    store: Arc<dyn RunStateStore + Send + Sync>,
    http: C,
    resolver: Arc<dyn CredentialResolver>,
}

impl RunExecutor {
    /// Executor de production : transport [`ReqwestHttpClient`] avec la
    /// politique egress de l'opérateur (`AUTOME_EGRESS_ALLOWLIST`).
    pub fn new(
        repo: Arc<dyn WorkflowRepository + Send + Sync>,
        store: Arc<dyn RunStateStore + Send + Sync>,
        resolver: Arc<dyn CredentialResolver>,
    ) -> Self {
        Self::with_http(
            repo,
            store,
            resolver,
            ReqwestHttpClient::with_policy(EgressPolicy::from_env()),
        )
    }
}

impl<C> RunExecutor<C>
where
    C: HttpClient + Clone + Send + Sync + 'static,
{
    /// Variante avec un transport HTTP explicite (tests sans réseau).
    pub fn with_http(
        repo: Arc<dyn WorkflowRepository + Send + Sync>,
        store: Arc<dyn RunStateStore + Send + Sync>,
        resolver: Arc<dyn CredentialResolver>,
        http: C,
    ) -> Self {
        Self {
            repo,
            store,
            http,
            resolver,
        }
    }
}

/// Horodatage Unix (ms) courant.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Runner de nodes de production. `http`/`resolver` sont clonés/partagés dans
/// le thread bloquant.
///
/// - `Trigger` : pass-through (`Null`) — le déclenchement a eu lieu côté API.
/// - `Action { http_request }` : délègue à [`HttpAction`] (les `params` du
///   node portent `url`/`method`/`headers`/`body`).
/// - `Action { slack.* | gmail.* | notion.* | github.* }` : délègue à
///   [`dispatch_action`] (injection du token de `owner`, propriétaire du run,
///   via `resolver`).
/// - autre `Action` : erreur explicite.
/// - `Switch`/`Loop` : gérés par le moteur, jamais soumis au runner.
///
/// `_ctx` est **volontairement inutilisé** : le templating inter-nodes (#160,
/// F2.6) est résolu **en amont** par le moteur (`run_sequential` →
/// `evaluate_node` → `mapping::resolve_params`), si bien que `node.params`
/// arrive **déjà résolu** ici (les `{{ <node>.<champ> }}` pointant sur les
/// sorties amont sont substitués avant l'appel au runner).
fn run_node<C: HttpClient + Clone>(
    node: &Node,
    _ctx: &RunContext,
    http: &C,
    resolver: &dyn CredentialResolver,
    owner: Option<&str>,
) -> Result<serde_json::Value, String> {
    match &node.kind {
        NodeKind::Trigger { .. } => Ok(serde_json::Value::Null),
        NodeKind::Action { action_type } => match action_type.as_str() {
            "http_request" => HttpAction::new(http.clone())
                .execute(&node.params)
                .map_err(|e| e.to_string()),
            other => dispatch_action(other, &node.params, http, resolver, owner),
        },
        other => Err(format!(
            "node de type {other:?} non exécutable par le runner"
        )),
    }
}

#[async_trait]
impl<C> Handler for RunExecutor<C>
where
    C: HttpClient + Clone + Send + Sync + 'static,
{
    async fn handle(&self, delivery: &Delivery) -> HandlerVerdict {
        let body = &delivery.message.body;
        let Some(run_id) = body["run_id"].as_str() else {
            eprintln!("[autome-worker] message sans `run_id` → DLQ");
            return HandlerVerdict::DeadLetter;
        };
        let Some(workflow_id) = body["workflow_id"].as_str() else {
            eprintln!("[autome-worker] message sans `workflow_id` → DLQ");
            return HandlerVerdict::DeadLetter;
        };

        // Charge le workflow puis le checkpoint courant.
        let workflow = match self.repo.get(&WorkflowId::new(workflow_id)).await {
            Ok(Some(wf)) => wf,
            Ok(None) => {
                eprintln!("[autome-worker] workflow `{workflow_id}` introuvable → DLQ");
                return HandlerVerdict::DeadLetter;
            }
            Err(e) => {
                eprintln!("[autome-worker] erreur repo workflow `{workflow_id}` : {e} → retry");
                return HandlerVerdict::Retry;
            }
        };
        let dag = match Dag::from_workflow(&workflow) {
            Ok(dag) => dag,
            Err(e) => {
                eprintln!("[autome-worker] DAG invalide pour `{workflow_id}` : {e} → DLQ");
                return HandlerVerdict::DeadLetter;
            }
        };
        let mut checkpoint = match self.store.load(run_id).await {
            Ok(Some(cp)) => cp,
            Ok(None) => {
                eprintln!("[autome-worker] run `{run_id}` introuvable → DLQ");
                return HandlerVerdict::DeadLetter;
            }
            Err(e) => {
                eprintln!("[autome-worker] erreur load run `{run_id}` : {e} → retry");
                return HandlerVerdict::Retry;
            }
        };

        // Transition Queued → Running.
        checkpoint.status = RunStatus::Running;
        checkpoint.started_at_ms = Some(now_ms());
        if let Err(e) = self.store.checkpoint(&checkpoint).await {
            eprintln!("[autome-worker] checkpoint Running échoué (`{run_id}`) : {e} → retry");
            return HandlerVerdict::Retry;
        }

        // Exécution **synchrone + bloquante** du moteur, hors runtime tokio.
        // Les tokens des connecteurs sont ceux du propriétaire du run (fixé au
        // dispatch par l'API, immuable).
        let http = self.http.clone();
        let resolver = Arc::clone(&self.resolver);
        let owner = checkpoint.owner_id.clone();
        let report = match tokio::task::spawn_blocking(move || {
            let runner = move |node: &Node, ctx: &RunContext| {
                run_node(node, ctx, &http, resolver.as_ref(), owner.as_deref())
            };
            Engine::new().run(&dag, &runner)
        })
        .await
        {
            Ok(report) => report,
            Err(e) => {
                eprintln!("[autome-worker] panic moteur (`{run_id}`) : {e} → retry");
                return HandlerVerdict::Retry;
            }
        };

        // Persiste les sorties des nodes réussis + le statut final.
        for step in &report.steps {
            if step.status == StepStatus::Succeeded {
                if let Some(out) = &step.output {
                    checkpoint
                        .outputs
                        .insert(step.node_id.0.clone(), out.clone());
                }
            }
        }
        checkpoint.status = report.status;
        checkpoint.finished_at_ms = Some(now_ms());
        if report.status == RunStatus::Failed {
            checkpoint.error = report
                .steps
                .iter()
                .find_map(|s| s.error.clone())
                .or_else(|| Some("run en échec".into()));
        }
        if let Err(e) = self.store.checkpoint(&checkpoint).await {
            // L'état final n'a pas pu être persisté : on retente la livraison
            // (le moteur sera rejoué — idempotent côté store last-write-wins).
            eprintln!("[autome-worker] checkpoint final échoué (`{run_id}`) : {e} → retry");
            return HandlerVerdict::Retry;
        }

        match report.status {
            RunStatus::Succeeded => HandlerVerdict::Ok,
            _ => HandlerVerdict::DeadLetter,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_connectors::oauth::{ConnectorTokenStore, InMemoryConnectorTokenStore, TokenSet};
    use autome_connectors::{HttpRequest, HttpResponse};
    use autome_core::{
        Edge, InMemoryRunStateStore, InMemoryWorkflowRepository, Message, NodeId, RunCheckpoint,
        WorkflowGraph,
    };
    use autome_core::{EnvelopeCipher, InMemoryKeyProvider};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    use crate::dispatch::OwnerCredentialResolver;

    const NOW: i64 = 1_700_000_000_000;

    /// Store chiffré : seule alice a connecté Slack.
    fn tokens_store() -> Arc<dyn ConnectorTokenStore + Send + Sync> {
        let store = InMemoryConnectorTokenStore::new(EnvelopeCipher::new(
            InMemoryKeyProvider::with_key("v1", [5u8; 32]),
        ));
        let alice = TokenSet {
            access_token: "xoxb-alice".into(),
            refresh_token: None,
            expires_at_ms: None,
            token_type: "Bearer".into(),
            scope: Some("chat:write".into()),
        };
        store.store("alice", "slack", &alice).unwrap();
        Arc::new(store)
    }

    /// Résolveur de production (mode `per_user`) sur [`tokens_store`].
    fn resolver() -> Arc<dyn CredentialResolver> {
        Arc::new(OwnerCredentialResolver::new(tokens_store()))
    }

    /// Transport HTTP clonable qui enregistre les requêtes (aucun réseau).
    #[derive(Clone, Default)]
    struct RecordingHttp {
        sent: Arc<Mutex<Vec<HttpRequest>>>,
    }

    impl HttpClient for RecordingHttp {
        fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
            self.sent.lock().unwrap().push(req.clone());
            Ok(HttpResponse {
                status: 200,
                body: json!({ "ok": true, "ts": "1.2" }),
            })
        }
    }

    fn slack_workflow(id: &str) -> WorkflowGraph {
        let mut wf = trigger_only(id);
        wf.nodes.push(Node {
            id: NodeId::new("notify"),
            name: "notify".into(),
            kind: NodeKind::Action {
                action_type: "slack.send_message".into(),
            },
            params: json!({ "channel": "C1", "text": "hello" }),
        });
        wf.edges.push(Edge {
            from: NodeId::new("entry"),
            to: NodeId::new("notify"),
            from_port: None,
        });
        wf
    }

    fn trigger_only(id: &str) -> WorkflowGraph {
        WorkflowGraph {
            id: WorkflowId::new(id),
            name: id.into(),
            nodes: vec![Node {
                id: NodeId::new("entry"),
                name: "entry".into(),
                kind: NodeKind::Trigger {
                    trigger_type: "manual".into(),
                },
                params: Value::Null,
            }],
            edges: vec![],
        }
    }

    fn unsupported_action(id: &str) -> WorkflowGraph {
        let mut wf = trigger_only(id);
        wf.nodes.push(Node {
            id: NodeId::new("act"),
            name: "act".into(),
            kind: NodeKind::Action {
                action_type: "slack_send".into(),
            },
            params: Value::Null,
        });
        wf.edges.push(Edge {
            from: NodeId::new("entry"),
            to: NodeId::new("act"),
            from_port: None,
        });
        wf
    }

    fn delivery(run_id: &str, workflow_id: &str) -> Delivery {
        use autome_core::DeliveryId;
        Delivery {
            id: DeliveryId::new(1),
            queue: "runs".into(),
            message: Message::new(json!({
                "run_id": run_id,
                "workflow_id": workflow_id,
                "version": 1,
                "trigger_type": "manual",
            })),
            redeliveries: 0,
        }
    }

    async fn seed(
        repo: &Arc<InMemoryWorkflowRepository>,
        store: &Arc<InMemoryRunStateStore>,
        wf: WorkflowGraph,
        run_id: &str,
    ) {
        seed_owned(repo, store, wf, run_id, None).await;
    }

    /// Comme [`seed`], avec le propriétaire porté par le workflow et le run.
    async fn seed_owned(
        repo: &Arc<InMemoryWorkflowRepository>,
        store: &Arc<InMemoryRunStateStore>,
        wf: WorkflowGraph,
        run_id: &str,
        owner: Option<&str>,
    ) {
        let wf_id = wf.id.0.clone();
        repo.create(wf, owner).await.unwrap();
        store
            .checkpoint(
                &RunCheckpoint::queued(run_id, &wf_id, 1, "manual", NOW)
                    .with_owner(owner.map(str::to_string)),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn trigger_only_run_succeeds_and_persists_status() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed(&repo, &store, trigger_only("wf-ok"), "run-ok").await;
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>, resolver());

        let verdict = exec.handle(&delivery("run-ok", "wf-ok")).await;
        assert_eq!(verdict, HandlerVerdict::Ok);
        let cp = store.load("run-ok").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Succeeded);
        assert!(cp.started_at_ms.is_some());
        assert!(cp.finished_at_ms.is_some());
    }

    #[tokio::test]
    async fn unsupported_action_fails_and_dead_letters() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed(&repo, &store, unsupported_action("wf-bad"), "run-bad").await;
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>, resolver());

        let verdict = exec.handle(&delivery("run-bad", "wf-bad")).await;
        assert_eq!(verdict, HandlerVerdict::DeadLetter);
        let cp = store.load("run-bad").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        assert!(cp.error.is_some());
    }

    #[tokio::test]
    async fn http_request_to_cloud_metadata_is_blocked_by_egress_guard() {
        // Le runner réel (ReqwestHttpClient + politique egress) refuse la
        // cible avant toute connexion : le run échoue avec le motif SSRF.
        let mut wf = trigger_only("wf-ssrf");
        wf.nodes.push(Node {
            id: NodeId::new("act"),
            name: "act".into(),
            kind: NodeKind::Action {
                action_type: "http_request".into(),
            },
            params: json!({
                "method": "GET",
                "url": "http://169.254.169.254/latest/meta-data/iam/security-credentials/"
            }),
        });
        wf.edges.push(Edge {
            from: NodeId::new("entry"),
            to: NodeId::new("act"),
            from_port: None,
        });
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed(&repo, &store, wf, "run-ssrf").await;
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>, resolver());

        let verdict = exec.handle(&delivery("run-ssrf", "wf-ssrf")).await;
        assert_eq!(verdict, HandlerVerdict::DeadLetter);
        let cp = store.load("run-ssrf").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        let error = cp.error.unwrap();
        assert!(error.contains("egress refusé"), "{error}");
    }

    #[tokio::test]
    async fn connector_uses_the_token_of_the_run_owner() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed_owned(
            &repo,
            &store,
            slack_workflow("wf-a"),
            "run-a",
            Some("alice"),
        )
        .await;
        let http = RecordingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());

        assert_eq!(
            exec.handle(&delivery("run-a", "wf-a")).await,
            HandlerVerdict::Ok
        );
        let auth = {
            let sent = http.sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            sent[0].headers.get("Authorization").cloned()
        };
        assert_eq!(auth.as_deref(), Some("Bearer xoxb-alice"));
        let cp = store.load("run-a").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn run_of_an_owner_without_token_fails_without_borrowing_another_token() {
        // bob n'a pas connecté Slack : son run ne doit jamais partir avec le
        // token d'alice ni avec un token global.
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed_owned(&repo, &store, slack_workflow("wf-b"), "run-b", Some("bob")).await;
        let http = RecordingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());

        assert_eq!(
            exec.handle(&delivery("run-b", "wf-b")).await,
            HandlerVerdict::DeadLetter
        );
        assert!(
            http.sent.lock().unwrap().is_empty(),
            "aucune requête envoyée"
        );
        let cp = store.load("run-b").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        let error = cp.error.unwrap();
        assert!(error.contains("propriétaire `bob`"), "{error}");
    }

    #[tokio::test]
    async fn connector_in_a_run_without_owner_fails_explicitly() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed_owned(&repo, &store, slack_workflow("wf-n"), "run-n", None).await;
        let http = RecordingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());

        assert_eq!(
            exec.handle(&delivery("run-n", "wf-n")).await,
            HandlerVerdict::DeadLetter
        );
        assert!(http.sent.lock().unwrap().is_empty());
        let error = store.load("run-n").await.unwrap().unwrap().error.unwrap();
        assert!(error.contains("sans propriétaire"), "{error}");
    }

    #[tokio::test]
    async fn unknown_workflow_dead_letters() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        // run présent mais workflow absent du repo.
        store
            .checkpoint(&RunCheckpoint::queued(
                "run-x",
                "wf-missing",
                1,
                "manual",
                NOW,
            ))
            .await
            .unwrap();
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>, resolver());
        assert_eq!(
            exec.handle(&delivery("run-x", "wf-missing")).await,
            HandlerVerdict::DeadLetter
        );
    }

    #[tokio::test]
    async fn malformed_message_dead_letters() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        let exec = RunExecutor::new(repo, store, resolver());
        use autome_core::DeliveryId;
        let bad = Delivery {
            id: DeliveryId::new(1),
            queue: "runs".into(),
            message: Message::new(json!({ "nope": true })),
            redeliveries: 0,
        };
        assert_eq!(exec.handle(&bad).await, HandlerVerdict::DeadLetter);
    }
}

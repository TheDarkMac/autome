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
//! - tout autre `action_type` : erreur (connecteurs Slack/Gmail/… = suite).
//!
//! Le moteur est **synchrone** et le transport HTTP est **bloquant**
//! (`reqwest::blocking`) : exécuter `Engine::run` directement sur un thread
//! worker de tokio paniquerait (« cannot start a runtime from within a
//! runtime »). On déporte donc toute l'exécution moteur dans
//! [`tokio::task::spawn_blocking`].

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use autome_connectors::{EgressPolicy, HttpAction, ReqwestHttpClient};
use autome_core::{
    Action, Delivery, Node, NodeKind, RunStateStore, RunStatus, WorkflowId, WorkflowRepository,
};
use autome_engine::{Dag, Engine, RunContext, StepStatus};

use crate::dispatch::{dispatch_action, CredentialResolver, EnvCredentialResolver};
use crate::{Handler, HandlerVerdict};

/// Handler de production : exécute un run de bout en bout et persiste son état.
///
/// Construit avec des `Arc<dyn …>` (repo/store sélectionnés par
/// `AUTOME_PERSISTENCE`), un [`ReqwestHttpClient`] clonable partagé entre
/// toutes les actions HTTP, et un [`CredentialResolver`] qui injecte les tokens
/// des connecteurs (#159).
pub struct RunExecutor {
    repo: Arc<dyn WorkflowRepository + Send + Sync>,
    store: Arc<dyn RunStateStore + Send + Sync>,
    http: ReqwestHttpClient,
    resolver: Arc<dyn CredentialResolver>,
}

impl RunExecutor {
    /// Construit l'executor sur un repo de workflows et un store d'état donnés.
    /// Résolveur de credentials par défaut : [`EnvCredentialResolver`]
    /// (`AUTOME_CONNECTOR_TOKEN_<PROVIDER>`).
    pub fn new(
        repo: Arc<dyn WorkflowRepository + Send + Sync>,
        store: Arc<dyn RunStateStore + Send + Sync>,
    ) -> Self {
        Self::with_resolver(repo, store, Arc::new(EnvCredentialResolver))
    }

    /// Variante avec un [`CredentialResolver`] explicite (tests, futur store).
    pub fn with_resolver(
        repo: Arc<dyn WorkflowRepository + Send + Sync>,
        store: Arc<dyn RunStateStore + Send + Sync>,
        resolver: Arc<dyn CredentialResolver>,
    ) -> Self {
        Self {
            repo,
            store,
            http: ReqwestHttpClient::with_policy(EgressPolicy::from_env()),
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
///   [`dispatch_action`] (injection du token via `resolver`).
/// - autre `Action` : erreur explicite.
/// - `Switch`/`Loop` : gérés par le moteur, jamais soumis au runner.
///
/// `_ctx` est **volontairement inutilisé** : le templating inter-nodes (#160,
/// F2.6) est résolu **en amont** par le moteur (`run_sequential` →
/// `evaluate_node` → `mapping::resolve_params`), si bien que `node.params`
/// arrive **déjà résolu** ici (les `{{ <node>.<champ> }}` pointant sur les
/// sorties amont sont substitués avant l'appel au runner).
fn run_node(
    node: &Node,
    _ctx: &RunContext,
    http: &ReqwestHttpClient,
    resolver: &dyn CredentialResolver,
) -> Result<serde_json::Value, String> {
    match &node.kind {
        NodeKind::Trigger { .. } => Ok(serde_json::Value::Null),
        NodeKind::Action { action_type } => match action_type.as_str() {
            "http_request" => HttpAction::new(http.clone())
                .execute(&node.params)
                .map_err(|e| e.to_string()),
            other => dispatch_action(other, &node.params, http, resolver),
        },
        other => Err(format!(
            "node de type {other:?} non exécutable par le runner"
        )),
    }
}

#[async_trait]
impl Handler for RunExecutor {
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
        let http = self.http.clone();
        let resolver = Arc::clone(&self.resolver);
        let report = match tokio::task::spawn_blocking(move || {
            let runner =
                move |node: &Node, ctx: &RunContext| run_node(node, ctx, &http, resolver.as_ref());
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
    use autome_core::{
        Edge, InMemoryRunStateStore, InMemoryWorkflowRepository, Message, NodeId, RunCheckpoint,
        WorkflowGraph,
    };
    use serde_json::{json, Value};

    const NOW: i64 = 1_700_000_000_000;

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
        let wf_id = wf.id.0.clone();
        repo.create(wf, None).await.unwrap();
        store
            .checkpoint(&RunCheckpoint::queued(run_id, &wf_id, 1, "manual", NOW))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn trigger_only_run_succeeds_and_persists_status() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed(&repo, &store, trigger_only("wf-ok"), "run-ok").await;
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>);

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
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>);

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
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>);

        let verdict = exec.handle(&delivery("run-ssrf", "wf-ssrf")).await;
        assert_eq!(verdict, HandlerVerdict::DeadLetter);
        let cp = store.load("run-ssrf").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        let error = cp.error.unwrap();
        assert!(error.contains("egress refusé"), "{error}");
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
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>);
        assert_eq!(
            exec.handle(&delivery("run-x", "wf-missing")).await,
            HandlerVerdict::DeadLetter
        );
    }

    #[tokio::test]
    async fn malformed_message_dead_letters() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        let exec = RunExecutor::new(repo, store);
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

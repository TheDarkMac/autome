//! Executor de **production** : le [`Handler`] que le binaire `autome-worker`
//! branche sur la file réelle (dette #107, constat 2).
//!
//! Il promeut en production la logique éprouvée par le test e2e (F9.4) :
//!
//! ```text
//! message {run_id, workflow_id}  →  RunStateStore.load (propriétaire)
//!                                →  WorkflowRepository.get (espace du propriétaire)
//!                                →  RunStateStore.try_claim (Queued → Running, atomique)
//!                                →  Engine.run (DAG + node runner)
//!                                →  RunStateStore (sorties + Succeeded/Failed)
//! ```
//!
//! Le **runner de nodes** traduit chaque node en effet concret :
//! - `Trigger` : renvoie le `payload` du message de run (corps d'un webhook,
//!   payload d'un run manuel — #202), référençable par `{{ <trigger>.… }}` ;
//! - `Action { action_type: "http_request" }` : [`HttpAction`] via un
//!   [`ReqwestHttpClient`] **partagé** (un seul client bloquant réutilisé),
//!   filtré anti-SSRF : destinations publiques seulement, sauf hôtes de
//!   l'allowlist opérateur `AUTOME_EGRESS_ALLOWLIST` ;
//! - `Action { slack.* | gmail.* | notion.* | github.* }` : connecteur, avec le
//!   token **du propriétaire du run** (`RunCheckpoint.owner_id`) résolu par le
//!   [`CredentialResolver`] ;
//! - tout autre `action_type` : erreur.
//!
//! ## Livraison *au plus une fois* des effets de bord (#174)
//!
//! La file livre *au moins une fois* (redélivrance après crash, perte de
//! connexion, autre réplique). Le run n'est exécuté que par le worker qui
//! **réclame** atomiquement son passage `Queued → Running` en base : une
//! redélivrance d'un run déjà `Running` ou terminé est `ack` sans
//! ré-exécution (pas de second message Slack). Un run interrompu (worker
//! tué) n'est donc pas rejoué : le balayage des orphelins
//! ([`reap_orphans`]) le passe `Failed` passé un délai.
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
    LEGACY_NAMESPACE,
};
use autome_engine::{Dag, Engine, RunContext, StepStatus};
use tracing::{debug, error, info, warn, Instrument};

use crate::dispatch::{dispatch_action, CredentialResolver};
use crate::shutdown::InFlightRuns;
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
    /// Runs réclamés et pas encore terminés (arrêt borné, #200).
    in_flight: Arc<InFlightRuns>,
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
            in_flight: Arc::new(InFlightRuns::default()),
        }
    }

    /// Runs en cours d'exécution (à passer à [`crate::drain`]).
    pub fn in_flight(&self) -> Arc<InFlightRuns> {
        Arc::clone(&self.in_flight)
    }

    /// Verdict d'une livraison dont le run n'a pas pu être réclamé : inconnu
    /// (DLQ), ou déjà pris / terminé (redélivrance ⇒ `ack` sans exécution).
    async fn unclaimed(&self, run_id: &str) -> HandlerVerdict {
        match self.store.load(run_id).await {
            Ok(None) => {
                error!(run_id, verdict = "dead_letter", "run introuvable");
                HandlerVerdict::DeadLetter
            }
            Ok(Some(cp)) if cp.status == RunStatus::Queued => {
                // Course improbable (remis en file entre-temps) : on retente.
                warn!(
                    run_id,
                    verdict = "retry",
                    "run encore en file après un claim refusé"
                );
                HandlerVerdict::Retry
            }
            Ok(Some(cp)) => {
                info!(
                    run_id,
                    status = ?cp.status,
                    verdict = "ack",
                    "livraison dupliquée ignorée : run déjà pris ou terminé"
                );
                HandlerVerdict::Ok
            }
            Err(e) => {
                warn!(run_id, error = %e, verdict = "retry", "lecture du run impossible");
                HandlerVerdict::Retry
            }
        }
    }
}

/// Clôt en `Failed` les runs `Running` démarrés il y a plus de `max_run_ms`
/// (worker tué, checkpoint final perdu…), et renvoie leur nombre.
///
/// Sûr entre répliques et face aux runs qui se terminent pendant le
/// balayage : [`RunStateStore::fail_if_running`] n'écrase jamais un run
/// devenu terminal. `max_run_ms` doit dépasser la durée du plus long run
/// légitime, sinon un run encore en cours serait marqué en échec (son
/// checkpoint final le corrigerait ensuite).
pub async fn reap_orphans(
    store: &dyn RunStateStore,
    now_ms: i64,
    max_run_ms: i64,
) -> autome_core::Result<usize> {
    let reason = format!(
        "run interrompu : toujours en cours après {} s (worker arrêté ?)",
        max_run_ms / 1_000
    );
    let mut reaped = 0;
    for orphan in store
        .list_orphans(now_ms.saturating_sub(max_run_ms))
        .await?
    {
        if store
            .fail_if_running(&orphan.run_id, &reason, now_ms)
            .await?
        {
            reaped += 1;
        }
    }
    Ok(reaped)
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
/// - `Trigger` : renvoie `payload`, celui du message de run (#202) — le
///   déclenchement a eu lieu côté API ; `Null` en l'absence de payload.
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
    payload: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    match &node.kind {
        NodeKind::Trigger { .. } => Ok(payload.clone()),
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
            error!(verdict = "dead_letter", "message sans `run_id`");
            return HandlerVerdict::DeadLetter;
        };
        let Some(workflow_id) = body["workflow_id"].as_str() else {
            error!(
                run_id,
                verdict = "dead_letter",
                "message sans `workflow_id`"
            );
            return HandlerVerdict::DeadLetter;
        };
        // Tous les logs du run (y compris ceux du moteur et des connecteurs)
        // portent `run_id` / `workflow_id`.
        let span = tracing::info_span!("run", run_id, workflow_id);
        let payload = body["payload"].clone();
        self.execute(run_id, workflow_id, payload)
            .instrument(span)
            .await
    }
}

impl<C> RunExecutor<C>
where
    C: HttpClient + Clone + Send + Sync + 'static,
{
    /// Exécute le run `run_id` de `workflow_id` (dans le span du run) ; le
    /// `payload` du message est la sortie des nœuds trigger.
    async fn execute(
        &self,
        run_id: &str,
        workflow_id: &str,
        payload: serde_json::Value,
    ) -> HandlerVerdict {
        // Le workflow vit dans l'espace d'ids de son propriétaire (#188), qui
        // est aussi celui du run (fixé au dispatch, immuable) : on lit d'abord
        // le run, sans le réclamer, pour savoir où chercher.
        let namespace = match self.store.load(run_id).await {
            // Livraison dupliquée d'un run déjà pris ou terminé : rien à
            // exécuter, inutile de relire le workflow (peut-être supprimé).
            Ok(Some(cp)) if cp.status != RunStatus::Queued => return self.unclaimed(run_id).await,
            Ok(Some(cp)) => cp.owner_id.unwrap_or_else(|| LEGACY_NAMESPACE.to_string()),
            Ok(None) => {
                error!(verdict = "dead_letter", "run introuvable");
                return HandlerVerdict::DeadLetter;
            }
            Err(e) => {
                warn!(error = %e, verdict = "retry", "lecture du run impossible");
                return HandlerVerdict::Retry;
            }
        };
        let workflow = match self
            .repo
            .get(&namespace, &WorkflowId::new(workflow_id))
            .await
        {
            Ok(Some(wf)) => wf,
            Ok(None) => {
                error!(verdict = "dead_letter", "workflow introuvable");
                return HandlerVerdict::DeadLetter;
            }
            Err(e) => {
                warn!(error = %e, verdict = "retry", "lecture du workflow impossible");
                return HandlerVerdict::Retry;
            }
        };
        let dag = match Dag::from_workflow(&workflow) {
            Ok(dag) => dag,
            Err(e) => {
                error!(error = %e, verdict = "dead_letter", "DAG invalide");
                return HandlerVerdict::DeadLetter;
            }
        };
        // Prise en charge atomique Queued → Running : une seule réplique
        // exécute le run, même si la livraison est dupliquée.
        let started_ms = now_ms();
        let mut checkpoint = match self.store.try_claim(run_id, started_ms).await {
            Ok(Some(cp)) => cp,
            Ok(None) => return self.unclaimed(run_id).await,
            Err(e) => {
                warn!(error = %e, verdict = "retry", "claim du run impossible");
                return HandlerVerdict::Retry;
            }
        };
        // En cours jusqu'à la fin de cette fonction (ou l'interruption de la
        // tâche) : un arrêt du worker sait quels runs clore (#200).
        let _in_flight = self.in_flight.track(run_id);
        debug!(owner = checkpoint.owner_id.as_deref(), "run réclamé");

        // Exécution **synchrone + bloquante** du moteur, hors runtime tokio.
        // Les tokens des connecteurs sont ceux du propriétaire du run (fixé au
        // dispatch par l'API, immuable).
        let http = self.http.clone();
        let resolver = Arc::clone(&self.resolver);
        let owner = checkpoint.owner_id.clone();
        // Le span n'est pas propagé automatiquement au thread bloquant.
        let span = tracing::Span::current();
        let report = match tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            let runner = move |node: &Node, ctx: &RunContext| {
                run_node(
                    node,
                    ctx,
                    &http,
                    resolver.as_ref(),
                    owner.as_deref(),
                    &payload,
                )
            };
            Engine::new().run(&dag, &runner)
        })
        .await
        {
            Ok(report) => report,
            Err(e) => {
                // Pas de rejeu : des nodes ont pu produire leurs effets avant
                // le panic. Le run est clos en échec, visible de l'utilisateur.
                error!(error = %e, verdict = "dead_letter", "panic du moteur, run clos en échec");
                let reason = format!("panic du moteur : {e}");
                if let Err(e) = self.store.fail_if_running(run_id, &reason, now_ms()).await {
                    error!(error = %e, "échec du run non persisté");
                }
                return HandlerVerdict::DeadLetter;
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
            // L'état final n'a pas pu être persisté. Le run reste `Running` en
            // base : la redélivrance ne le rejouera pas (déjà réclamé), et le
            // balayage des orphelins le clora en échec.
            error!(error = %e, verdict = "retry", "état final du run non persisté");
            return HandlerVerdict::Retry;
        }

        let duration_ms = now_ms().saturating_sub(started_ms);
        match report.status {
            RunStatus::Succeeded => {
                info!(status = "succeeded", duration_ms, "run terminé");
                HandlerVerdict::Ok
            }
            status => {
                warn!(
                    status = ?status,
                    duration_ms,
                    error = checkpoint.error.as_deref(),
                    verdict = "dead_letter",
                    "run en échec"
                );
                HandlerVerdict::DeadLetter
            }
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
        repo.create(owner.unwrap_or(LEGACY_NAMESPACE), wf)
            .await
            .unwrap();
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

    // ───── Payload du run (#202) ─────

    #[tokio::test]
    async fn trigger_outputs_the_run_payload_for_downstream_templates() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        let mut wf = slack_workflow("wf-p");
        wf.nodes[1].params = json!({ "channel": "C1", "text": "{{ entry.body.issue.title }}" });
        seed_owned(&repo, &store, wf, "run-p", Some("alice")).await;
        let http = RecordingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());

        let mut msg = delivery("run-p", "wf-p");
        msg.message.body["payload"] = json!({ "body": { "issue": { "title": "Bug 42" } } });
        assert_eq!(exec.handle(&msg).await, HandlerVerdict::Ok);

        {
            let sent = http.sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].body.as_ref().unwrap()["text"], "Bug 42");
        }
        let cp = store.load("run-p").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Succeeded);
        assert_eq!(cp.outputs["entry"]["body"]["issue"]["title"], "Bug 42");
    }

    // ───── Livraison au plus une fois (#174) ─────

    #[tokio::test]
    async fn duplicate_delivery_of_a_finished_run_does_not_execute_again() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed_owned(
            &repo,
            &store,
            slack_workflow("wf-d"),
            "run-d",
            Some("alice"),
        )
        .await;
        let http = RecordingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());

        assert_eq!(
            exec.handle(&delivery("run-d", "wf-d")).await,
            HandlerVerdict::Ok
        );
        let finished = store.load("run-d").await.unwrap().unwrap();
        // Redélivrance (autre réplique, ack perdu…) : ack sans ré-exécution.
        assert_eq!(
            exec.handle(&delivery("run-d", "wf-d")).await,
            HandlerVerdict::Ok
        );
        assert_eq!(http.sent.lock().unwrap().len(), 1, "un seul message Slack");
        assert_eq!(store.load("run-d").await.unwrap().unwrap(), finished);
    }

    #[tokio::test]
    async fn delivery_of_a_run_claimed_elsewhere_is_acked_without_running() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed_owned(
            &repo,
            &store,
            slack_workflow("wf-r"),
            "run-r",
            Some("alice"),
        )
        .await;
        // Une autre réplique a déjà réclamé le run.
        let claimed = store.try_claim("run-r", NOW).await.unwrap().unwrap();
        let http = RecordingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());

        assert_eq!(
            exec.handle(&delivery("run-r", "wf-r")).await,
            HandlerVerdict::Ok
        );
        assert!(http.sent.lock().unwrap().is_empty());
        assert_eq!(store.load("run-r").await.unwrap().unwrap(), claimed);
    }

    #[tokio::test]
    async fn unknown_run_dead_letters() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        repo.create(LEGACY_NAMESPACE, trigger_only("wf-u"))
            .await
            .unwrap();
        let exec = RunExecutor::new(repo, Arc::new(InMemoryRunStateStore::new()), resolver());
        assert_eq!(
            exec.handle(&delivery("run-ghost", "wf-u")).await,
            HandlerVerdict::DeadLetter
        );
    }

    /// Transport qui panique : simule un bug dans un node.
    #[derive(Clone)]
    struct PanickingHttp;
    impl HttpClient for PanickingHttp {
        fn send(&self, _req: &HttpRequest) -> Result<HttpResponse, String> {
            panic!("bug simulé dans le transport");
        }
    }

    #[tokio::test]
    async fn engine_panic_fails_the_run_instead_of_replaying_it() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed_owned(
            &repo,
            &store,
            slack_workflow("wf-p"),
            "run-p",
            Some("alice"),
        )
        .await;
        let exec = RunExecutor::with_http(
            repo,
            Arc::clone(&store) as Arc<_>,
            resolver(),
            PanickingHttp,
        );

        assert_eq!(
            exec.handle(&delivery("run-p", "wf-p")).await,
            HandlerVerdict::DeadLetter
        );
        let cp = store.load("run-p").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        assert!(cp.error.unwrap().contains("panic du moteur"));
    }

    #[tokio::test]
    async fn reap_orphans_fails_only_stale_running_runs() {
        let store = InMemoryRunStateStore::new();
        let max = 3_600_000;
        for (id, status, started) in [
            ("stale", RunStatus::Running, NOW - max - 1),
            ("recent", RunStatus::Running, NOW - 1_000),
            ("done", RunStatus::Succeeded, NOW - max - 1),
        ] {
            let mut cp = RunCheckpoint::queued(id, "wf", 1, "manual", 0);
            cp.status = status;
            cp.started_at_ms = Some(started);
            store.checkpoint(&cp).await.unwrap();
        }

        assert_eq!(reap_orphans(&store, NOW, max).await.unwrap(), 1);
        let stale = store.load("stale").await.unwrap().unwrap();
        assert_eq!(stale.status, RunStatus::Failed);
        assert!(stale.error.unwrap().contains("3600 s"));
        assert_eq!(
            store.load("recent").await.unwrap().unwrap().status,
            RunStatus::Running
        );
        assert_eq!(
            store.load("done").await.unwrap().unwrap().status,
            RunStatus::Succeeded
        );
        // Idempotent : un second balayage ne trouve plus rien.
        assert_eq!(reap_orphans(&store, NOW, max).await.unwrap(), 0);
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
    async fn run_never_executes_a_homonymous_workflow_of_another_namespace() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        // `wf-s` n'existe que chez alice ; le run appartient à bob (#188).
        repo.create("alice", slack_workflow("wf-s")).await.unwrap();
        store
            .checkpoint(
                &RunCheckpoint::queued("run-bob", "wf-s", 1, "manual", NOW)
                    .with_owner(Some("bob".into())),
            )
            .await
            .unwrap();
        let http = RecordingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());
        assert_eq!(
            exec.handle(&delivery("run-bob", "wf-s")).await,
            HandlerVerdict::DeadLetter
        );
        assert!(http.sent.lock().unwrap().is_empty(), "rien n'est exécuté");
    }

    #[tokio::test]
    async fn duplicate_delivery_is_acked_even_if_the_workflow_was_deleted() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed_owned(&repo, &store, trigger_only("wf-g"), "run-g", Some("alice")).await;
        let exec = RunExecutor::new(
            Arc::clone(&repo) as Arc<_>,
            Arc::clone(&store) as Arc<_>,
            resolver(),
        );
        assert_eq!(
            exec.handle(&delivery("run-g", "wf-g")).await,
            HandlerVerdict::Ok
        );
        repo.delete("alice", &WorkflowId::new("wf-g"))
            .await
            .unwrap();
        // Avant : workflow introuvable ⇒ lettre morte pour un run déjà terminé.
        assert_eq!(
            exec.handle(&delivery("run-g", "wf-g")).await,
            HandlerVerdict::Ok
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

    // ───── Arrêt borné (#200) ─────

    /// Transport qui met la durée donnée à répondre : un run « long ».
    #[derive(Clone)]
    struct SlowHttp(std::time::Duration);
    impl HttpClient for SlowHttp {
        fn send(&self, _req: &HttpRequest) -> Result<HttpResponse, String> {
            std::thread::sleep(self.0);
            Ok(HttpResponse {
                status: 200,
                body: json!({ "ok": true }),
            })
        }
    }

    fn http_workflow(id: &str) -> WorkflowGraph {
        let mut wf = trigger_only(id);
        wf.nodes.push(Node {
            id: NodeId::new("call"),
            name: "call".into(),
            kind: NodeKind::Action {
                action_type: "http_request".into(),
            },
            params: json!({ "method": "GET", "url": "https://example.com/slow" }),
        });
        wf.edges.push(Edge {
            from: NodeId::new("entry"),
            to: NodeId::new("call"),
            from_port: None,
        });
        wf
    }

    /// Pool d'un consommateur avec un run « long » en file, démarré et
    /// attendu jusqu'à ce que le run soit réclamé.
    async fn running_pool_with_slow_run(
        delay: std::time::Duration,
    ) -> (
        crate::RunningPool,
        Arc<crate::InFlightRuns>,
        Arc<InMemoryRunStateStore>,
    ) {
        use autome_core::{InMemoryQueue, Queue};
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed(&repo, &store, http_workflow("wf-slow"), "run-slow").await;
        let queue = Arc::new(InMemoryQueue::new());
        queue
            .publish("runs", delivery("run-slow", "wf-slow").message)
            .await
            .unwrap();
        let exec = RunExecutor::with_http(
            repo,
            Arc::clone(&store) as Arc<_>,
            resolver(),
            SlowHttp(delay),
        );
        let in_flight = exec.in_flight();
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(exec);
        let idem: Arc<dyn crate::IdempotencyStore + Send + Sync> =
            Arc::new(crate::InMemoryIdempotencyStore::new());
        let running = crate::WorkerPool::new(queue, handler, idem, "runs", 1)
            .with_idle_sleep(std::time::Duration::from_millis(10))
            .start();
        for _ in 0..200 {
            if !in_flight.snapshot().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(in_flight.snapshot(), vec!["run-slow"], "run réclamé");
        (running, in_flight, store)
    }

    #[tokio::test]
    async fn drain_lets_in_flight_runs_finish_within_the_timeout() {
        let (running, in_flight, store) =
            running_pool_with_slow_run(std::time::Duration::from_millis(200)).await;
        let outcome = crate::drain(
            running,
            &in_flight,
            store.as_ref(),
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(outcome, crate::DrainOutcome::Completed);
        let cp = store.load("run-slow").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Succeeded);
        assert!(in_flight.snapshot().is_empty());
    }

    #[tokio::test]
    async fn drain_past_the_timeout_fails_the_interrupted_runs_at_once() {
        let (running, in_flight, store) =
            running_pool_with_slow_run(std::time::Duration::from_secs(3)).await;
        let started = std::time::Instant::now();
        let outcome = crate::drain(
            running,
            &in_flight,
            store.as_ref(),
            std::time::Duration::from_millis(200),
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "borné"
        );
        assert_eq!(
            outcome,
            crate::DrainOutcome::Interrupted {
                failed: vec!["run-slow".into()]
            }
        );
        // Clos tout de suite, avec un motif explicite — pas `running` pendant
        // une heure jusqu'au balayage des orphelins.
        let cp = store.load("run-slow").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        assert_eq!(
            cp.error.as_deref(),
            Some(crate::shutdown::INTERRUPTED_REASON)
        );
        assert!(cp.finished_at_ms.is_some());
    }

    #[tokio::test]
    async fn redelivery_of_an_interrupted_run_is_acked_without_running() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed(&repo, &store, trigger_only("wf-i"), "run-i").await;
        store.try_claim("run-i", NOW).await.unwrap().unwrap();
        assert!(store
            .fail_if_running("run-i", crate::shutdown::INTERRUPTED_REASON, NOW + 1)
            .await
            .unwrap());
        let http = RecordingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());
        assert_eq!(
            exec.handle(&delivery("run-i", "wf-i")).await,
            HandlerVerdict::Ok
        );
        assert_eq!(
            store.load("run-i").await.unwrap().unwrap().status,
            RunStatus::Failed,
            "pas de rejeu"
        );
    }
}

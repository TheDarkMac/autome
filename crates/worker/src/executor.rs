//! Executor de **production** : le [`Handler`] que le binaire `autome-worker`
//! branche sur la file réelle (dette #107, constat 2).
//!
//! Il promeut en production la logique éprouvée par le test e2e (F9.4) :
//!
//! ```text
//! message {run_id, workflow_id}  →  RunStateStore.load (propriétaire)
//!                                →  WorkflowRepository.get (espace du propriétaire)
//!                                →  RunStateStore.try_claim (Queued → Running, tentative + 1)
//!                                →  resume_sequential (DAG + node runner, depuis le
//!                                   point de reprise ; progression avant / après
//!                                   chaque nœud, battement)
//!                                →  RunStateStore.finish (sorties + Succeeded/Failed)
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
//! ré-exécution (pas de second message Slack).
//!
//! ## Reprise des runs interrompus (#239)
//!
//! Avant chaque nœud, le worker l'enregistre comme **nœud en cours**
//! (`next_node`) ; après, il enregistre le **point de reprise** du moteur
//! (`resume_state`) et efface le nœud en cours. Un nœud ne démarre jamais
//! sans ces deux écritures. Un run interrompu — arrêt du worker (il s'arrête
//! au prochain nœud), battement expiré (worker tué) — est remis en file et
//! repris par la tentative suivante à son point de reprise, sans rien
//! rejouer. Si un nœud était en cours, son effet est incertain : il n'est
//! rejoué que s'il est rejouable (`autome_engine::rerunnable`), sinon le run
//! est clos en échec avec un motif qui le nomme. Toutes les écritures sont
//! gardées par la tentative : un worker qui a perdu la main n'écrit plus
//! rien et s'arrête au prochain nœud.
//!
//! Le moteur est **synchrone** et le transport HTTP est **bloquant**
//! (`reqwest::blocking`) : exécuter `Engine::run` directement sur un thread
//! worker de tokio paniquerait (« cannot start a runtime from within a
//! runtime »). On déporte donc toute l'exécution moteur dans
//! [`tokio::task::spawn_blocking`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use autome_connectors::{EgressPolicy, HttpAction, HttpClient, ReqwestHttpClient};
use autome_core::{
    telemetry, Action, Delivery, Node, NodeId, NodeKind, Queue, RunCheckpoint, RunProgress,
    RunStateStore, RunStatus, WorkflowId, WorkflowRepository, LEGACY_NAMESPACE,
};
use autome_engine::{
    rerunnable, resume_sequential, Control, Cursor, Dag, Execution, RunContext, RunObserver,
    StepStatus,
};
use tracing::{debug, error, info, warn, Instrument};

use crate::dispatch::{dispatch_action, CredentialResolver};
use crate::metrics::{RequeueReason, RunOutcome, WorkerMetrics};
use crate::outbox::{publish, DEFAULT_LEASE};
use crate::shutdown::{InFlightRuns, INTERRUPTED_REASON};
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
    /// Runs terminés et leur durée (#235).
    metrics: Arc<WorkerMetrics>,
    /// Période du battement des runs en cours (un quart du bail, #239).
    heartbeat_every: Duration,
    /// File où republier un run interrompu ; sans elle, le relais de
    /// l'outbox s'en charge.
    queue: Option<Arc<dyn Queue + Send + Sync>>,
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
            metrics: Arc::new(WorkerMetrics::new()),
            heartbeat_every: DEFAULT_LEASE / 4,
            queue: None,
        }
    }

    /// Builder : bail des runs (#239) — le worker bat tous les quarts de bail.
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.heartbeat_every = (lease / 4).max(Duration::from_millis(10));
        self
    }

    /// Builder : file où republier tout de suite un run interrompu (#239).
    pub fn with_queue(mut self, queue: Arc<dyn Queue + Send + Sync>) -> Self {
        self.queue = Some(queue);
        self
    }

    /// Builder : registre où compter les runs terminés (#235) ; par défaut
    /// un registre propre à l'executor.
    pub fn with_metrics(mut self, metrics: Arc<WorkerMetrics>) -> Self {
        self.metrics = metrics;
        self
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
        // portent `run_id` / `workflow_id`. En trace (#249), le run est
        // l'enfant de sa mise en file (`traceparent` du message, conservé
        // par l'outbox et les remises en file) et ses logs portent
        // `trace_id`.
        let span = tracing::info_span!(
            "run",
            otel.kind = "consumer",
            otel.status_code = tracing::field::Empty,
            run_id,
            workflow_id,
            trace_id = tracing::field::Empty,
        );
        telemetry::set_parent(&span, &delivery.message.headers);
        telemetry::record_trace_id(&span);
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
        let started = Instant::now();
        let checkpoint = match self.store.try_claim(run_id, started_ms).await {
            Ok(Some(cp)) => cp,
            Ok(None) => return self.unclaimed(run_id).await,
            Err(e) => {
                warn!(error = %e, verdict = "retry", "claim du run impossible");
                return HandlerVerdict::Retry;
            }
        };
        let attempt = checkpoint.attempt;
        // En cours jusqu'à la fin de cette fonction (ou l'interruption de la
        // tâche) : un arrêt du worker sait quels runs remettre en file (#200,
        // #239).
        let _in_flight = self.in_flight.track(run_id, attempt);
        debug!(
            owner = checkpoint.owner_id.as_deref(),
            attempt, "run réclamé"
        );

        // Point de reprise d'un run interrompu (#239), ou départ.
        let cursor = match resume_point(&dag, &checkpoint) {
            Ok(cursor) => cursor,
            Err(reason) => return self.abandon(checkpoint, reason, started).await,
        };
        if checkpoint.resume_state.is_some() {
            info!(
                attempt,
                done = cursor.done(),
                "reprise du run à son point de reprise"
            );
            self.metrics.run_resumed();
        }

        // Battement tant que le moteur tourne ; s'il est refusé, le run a été
        // repris ailleurs : le moteur s'arrête au prochain nœud.
        let lost = Arc::new(AtomicBool::new(false));
        let heartbeat = tokio::spawn(beat(
            Arc::clone(&self.store),
            run_id.to_string(),
            attempt,
            self.heartbeat_every,
            Arc::clone(&lost),
        ));

        // Exécution **synchrone + bloquante** du moteur, hors runtime tokio.
        // Les tokens des connecteurs sont ceux du propriétaire du run (fixé au
        // dispatch par l'API, immuable).
        let http = self.http.clone();
        let resolver = Arc::clone(&self.resolver);
        let owner = checkpoint.owner_id.clone();
        let observer = ProgressObserver {
            store: Arc::clone(&self.store),
            runtime: tokio::runtime::Handle::current(),
            run_id: run_id.to_string(),
            attempt,
            in_flight: Arc::clone(&self.in_flight),
            lost: Arc::clone(&lost),
            unsaved: Mutex::new(None),
        };
        // Le span n'est pas propagé automatiquement au thread bloquant, ni le
        // subscriber s'il n'est pas global (tests) : les spans des nœuds en
        // dépendent (#249).
        let span = tracing::Span::current();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let execution = tokio::task::spawn_blocking(move || {
            let _subscriber = tracing::dispatcher::set_default(&dispatch);
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
            resume_sequential(&dag, &runner, cursor, &observer)
        })
        .await;
        heartbeat.abort();

        let report = match execution {
            Ok(Ok(Execution::Finished(report))) => report,
            Ok(Ok(Execution::Interrupted { cursor, next })) => {
                return self
                    .interrupted(
                        run_id,
                        attempt,
                        &cursor,
                        &next,
                        lost.load(Ordering::Acquire),
                    )
                    .await;
            }
            // Curseur vérifié par `resume_point` : ne devrait pas arriver.
            Ok(Err(reason)) => return self.abandon(checkpoint, reason, started).await,
            Err(e) => {
                // Pas de rejeu : des nodes ont pu produire leurs effets avant
                // le panic. Le run est clos en échec, visible de l'utilisateur.
                error!(error = %e, verdict = "dead_letter", "panic du moteur, run clos en échec");
                let reason = format!("panic du moteur : {e}");
                if let Err(e) = self.store.fail_if_running(run_id, &reason, now_ms()).await {
                    error!(error = %e, "échec du run non persisté");
                }
                self.metrics
                    .run_finished(RunOutcome::Failed, started.elapsed());
                return HandlerVerdict::DeadLetter;
            }
        };

        // Persiste les sorties des nodes réussis + le statut final.
        let mut checkpoint = checkpoint;
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
        checkpoint.next_node = None;
        checkpoint.resume_state = None;
        if report.status == RunStatus::Failed {
            checkpoint.error = report
                .steps
                .iter()
                .find_map(|s| s.error.clone())
                .or_else(|| Some("run en échec".into()));
        }
        match self.store.finish(&checkpoint).await {
            Ok(true) => {}
            // Run effacé pendant son exécution (suppression du compte de son
            // propriétaire, #229), ou repris par un autre worker après la
            // perte du battement (#239) : rien n'est réécrit, la livraison est
            // acquittée.
            Ok(false) => {
                info!(
                    verdict = "ack",
                    "run effacé ou repris ailleurs pendant son exécution, état final abandonné"
                );
                return HandlerVerdict::Ok;
            }
            Err(e) => {
                // L'état final n'a pas pu être persisté. Le run reste `Running`
                // avec son point de reprise complet : sans battement, il sera
                // remis en file et repris sans rien réexécuter, puis clos.
                error!(error = %e, verdict = "retry", "état final du run non persisté");
                return HandlerVerdict::Retry;
            }
        }

        let duration_ms = now_ms().saturating_sub(started_ms);
        let outcome = if report.status == RunStatus::Succeeded {
            RunOutcome::Succeeded
        } else {
            RunOutcome::Failed
        };
        self.metrics.run_finished(outcome, started.elapsed());
        match report.status {
            RunStatus::Succeeded => {
                info!(status = "succeeded", duration_ms, "run terminé");
                HandlerVerdict::Ok
            }
            status => {
                tracing::Span::current().record("otel.status_code", "error");
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

    /// Clôt en échec un run réclamé qui ne peut pas être (re)pris, avec les
    /// sorties déjà produites.
    async fn abandon(
        &self,
        mut checkpoint: RunCheckpoint,
        reason: String,
        started: Instant,
    ) -> HandlerVerdict {
        checkpoint.outputs = checkpoint
            .resume_state
            .as_ref()
            .and_then(|v| serde_json::from_value::<Cursor>(v.clone()).ok())
            .map(|c| c.outputs())
            .unwrap_or_default();
        checkpoint.status = RunStatus::Failed;
        checkpoint.error = Some(reason);
        checkpoint.finished_at_ms = Some(now_ms());
        checkpoint.next_node = None;
        checkpoint.resume_state = None;
        match self.store.finish(&checkpoint).await {
            Ok(true) => {}
            Ok(false) => {
                info!(verdict = "ack", "run effacé ou repris ailleurs entre-temps");
                return HandlerVerdict::Ok;
            }
            Err(e) => {
                error!(error = %e, verdict = "retry", "échec du run non persisté");
                return HandlerVerdict::Retry;
            }
        }
        self.metrics
            .run_finished(RunOutcome::Failed, started.elapsed());
        warn!(
            error = checkpoint.error.as_deref(),
            verdict = "dead_letter",
            "run clos en échec à la reprise"
        );
        HandlerVerdict::DeadLetter
    }

    /// Le moteur s'est arrêté avant `next` (arrêt du worker, battement perdu,
    /// progression impossible à enregistrer) : point de reprise enregistré et
    /// run remis en file, sauf s'il n'est plus à cette tentative.
    async fn interrupted(
        &self,
        run_id: &str,
        attempt: i32,
        cursor: &Cursor,
        next: &NodeId,
        lost: bool,
    ) -> HandlerVerdict {
        if lost {
            info!(
                verdict = "ack",
                next = %next.0,
                "run repris ailleurs ou clos : exécution arrêtée"
            );
            return HandlerVerdict::Ok;
        }
        let progress = RunProgress::Done {
            resume_state: serde_json::to_value(cursor).expect("curseur sérialisable"),
        };
        match self
            .store
            .record_progress(run_id, attempt, &progress, now_ms())
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                info!(
                    verdict = "ack",
                    "run repris ailleurs ou clos : exécution arrêtée"
                );
                return HandlerVerdict::Ok;
            }
            // Le dernier point de reprise enregistré reste valable (au pire
            // il désigne un nœud en cours, tranché à la reprise).
            Err(e) => warn!(error = %e, "point de reprise final non enregistré"),
        }
        match self.store.requeue(run_id, attempt, now_ms()).await {
            Ok(Some(message)) => {
                self.metrics.runs_requeued(RequeueReason::Shutdown, 1);
                info!(
                    verdict = "ack",
                    next = %next.0,
                    "run interrompu entre deux nœuds : remis en file"
                );
                match &self.queue {
                    Some(queue) => {
                        publish(queue.as_ref(), self.store.as_ref(), &message, now_ms()).await;
                    }
                    None => debug!("message laissé au relais de l'outbox"),
                }
            }
            Ok(None) => {
                // Pas de message (run antérieur à l'outbox) : clos en échec
                // comme avant #239, s'il est toujours à cette tentative.
                let still_ours = matches!(
                    self.store.load(run_id).await,
                    Ok(Some(cp)) if cp.status == RunStatus::Running && cp.attempt == attempt
                );
                if still_ours {
                    if let Err(e) = self
                        .store
                        .fail_if_running(run_id, INTERRUPTED_REASON, now_ms())
                        .await
                    {
                        error!(error = %e, "run interrompu non clos");
                    }
                }
            }
            // Sans battement, le run sera remis en file par un autre worker.
            Err(e) => error!(error = %e, "run interrompu non remis en file"),
        }
        HandlerVerdict::Ok
    }
}

/// Nombre maximal de tentatives d'un run : au-delà, il est clos en échec
/// plutôt que repris (arrêts répétés, nœud qui fait tomber le worker).
pub const MAX_RUN_ATTEMPTS: i32 = 10;

/// Point de départ de la tentative `checkpoint.attempt` : le début du graphe,
/// ou le point de reprise d'un run interrompu. `Err(motif)` si le run ne doit
/// pas être repris : trop de tentatives, workflow modifié depuis
/// l'interruption, ou nœud interrompu en cours d'exécution et non rejouable
/// (son effet est incertain — livraison *au plus une fois*, #174).
fn resume_point(dag: &Dag, checkpoint: &RunCheckpoint) -> Result<Cursor, String> {
    if checkpoint.attempt > MAX_RUN_ATTEMPTS {
        return Err(format!(
            "run abandonné après {MAX_RUN_ATTEMPTS} tentatives interrompues"
        ));
    }
    let cursor = match &checkpoint.resume_state {
        None => Cursor::start(dag),
        Some(state) => serde_json::from_value::<Cursor>(state.clone())
            .map_err(|e| format!("point de reprise illisible : {e}"))?,
    };
    if !cursor.fits(dag) {
        return Err(
            "workflow modifié depuis l'interruption du run : reprise impossible, à relancer".into(),
        );
    }
    if let Some(node) = &checkpoint.next_node {
        let rerun = dag.node(&NodeId::new(node.clone())).is_some_and(rerunnable);
        if !rerun {
            return Err(format!(
                "run interrompu pendant le nœud `{node}` : son effet est incertain, \
                 il n'est pas rejoué (à relancer)"
            ));
        }
    }
    Ok(cursor)
}

/// Bat pour la tentative `attempt` toutes les `every` ; s'arrête (et lève
/// `lost`) dès que le battement est refusé.
async fn beat(
    store: Arc<dyn RunStateStore + Send + Sync>,
    run_id: String,
    attempt: i32,
    every: Duration,
    lost: Arc<AtomicBool>,
) {
    let mut tick = tokio::time::interval(every);
    tick.tick().await; // immédiat : le claim vient de battre
    loop {
        tick.tick().await;
        match store.heartbeat(&run_id, attempt, now_ms()).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(run_id, "battement refusé : run repris ailleurs ou clos");
                lost.store(true, Ordering::Release);
                return;
            }
            Err(e) => warn!(run_id, error = %e, "battement du run impossible"),
        }
    }
}

/// Enregistre la progression du run autour de chaque nœud (#239), depuis le
/// thread bloquant du moteur.
///
/// Garantie *au plus une fois* : un nœud ne s'exécute qu'une fois marqué « en
/// cours » en base, et seulement si le point de reprise précédent est
/// enregistré — sinon le run s'arrête là. Après une interruption, un nœud
/// marqué en cours a donc pu produire son effet ; un nœud non marqué, non.
struct ProgressObserver {
    store: Arc<dyn RunStateStore + Send + Sync>,
    runtime: tokio::runtime::Handle,
    run_id: String,
    attempt: i32,
    in_flight: Arc<InFlightRuns>,
    /// Battement refusé : le run n'est plus à cette tentative.
    lost: Arc<AtomicBool>,
    /// Point de reprise qui n'a pas pu être enregistré.
    unsaved: Mutex<Option<serde_json::Value>>,
}

impl ProgressObserver {
    fn record(&self, progress: &RunProgress) -> autome_core::Result<bool> {
        self.runtime.block_on(self.store.record_progress(
            &self.run_id,
            self.attempt,
            progress,
            now_ms(),
        ))
    }

    fn unsaved(&self) -> std::sync::MutexGuard<'_, Option<serde_json::Value>> {
        self.unsaved.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl RunObserver for ProgressObserver {
    fn before_node(&self, node: &NodeId, _cursor: &Cursor) -> Control {
        if self.lost.load(Ordering::Acquire) {
            return Control::Stop;
        }
        if self.in_flight.stop_requested() {
            info!(next = %node.0, "arrêt du worker : run interrompu avant le nœud");
            return Control::Stop;
        }
        let unsaved = self.unsaved().take();
        if let Some(resume_state) = unsaved {
            if !self.saved(&RunProgress::Done { resume_state }) {
                return Control::Stop;
            }
        }
        let starting = RunProgress::Starting {
            node: node.0.clone(),
        };
        if self.saved(&starting) {
            Control::Continue
        } else {
            Control::Stop
        }
    }

    fn after_node(&self, cursor: &Cursor) {
        let resume_state = serde_json::to_value(cursor).expect("curseur sérialisable");
        let done = RunProgress::Done {
            resume_state: resume_state.clone(),
        };
        if !self.saved(&done) && !self.lost.load(Ordering::Acquire) {
            // Réessayé avant le nœud suivant, qui ne démarre pas sans lui.
            *self.unsaved() = Some(resume_state);
        }
    }
}

impl ProgressObserver {
    /// Enregistre `progress` ; `false` si c'est impossible (base injoignable)
    /// ou refusé (run plus à cette tentative, `lost` levé).
    fn saved(&self, progress: &RunProgress) -> bool {
        match self.record(progress) {
            Ok(true) => true,
            Ok(false) => {
                self.lost.store(true, Ordering::Release);
                false
            }
            Err(e) => {
                warn!(error = %e, "progression du run non enregistrée");
                false
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
            position: None,
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
                position: None,
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
            position: None,
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

    // `the_run_continues_the_trace_of_its_dispatch` vit dans
    // `tests/run_trace.rs`, seul dans son binaire (#259).

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
    async fn finished_runs_are_counted_by_status_but_duplicates_are_not() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed(&repo, &store, trigger_only("wf-ok"), "run-ok").await;
        seed(&repo, &store, unsupported_action("wf-bad"), "run-bad").await;
        let metrics = Arc::new(WorkerMetrics::new());
        let exec = RunExecutor::new(repo, Arc::clone(&store) as Arc<_>, resolver())
            .with_metrics(Arc::clone(&metrics));

        exec.handle(&delivery("run-ok", "wf-ok")).await;
        exec.handle(&delivery("run-bad", "wf-bad")).await;
        // Redélivrance d'un run terminé : acquittée, pas comptée comme run.
        assert_eq!(
            exec.handle(&delivery("run-ok", "wf-ok")).await,
            HandlerVerdict::Ok
        );

        let out = metrics.render(crate::metrics::Gauges::default());
        assert!(out.contains("autome_worker_runs_total{status=\"succeeded\"} 1\n"));
        assert!(out.contains("autome_worker_runs_total{status=\"failed\"} 1\n"));
        assert!(out.contains("autome_worker_run_duration_seconds_count{status=\"succeeded\"} 1\n"));
    }

    #[tokio::test]
    async fn http_request_to_cloud_metadata_is_blocked_by_egress_guard() {
        // Le runner réel (ReqwestHttpClient + politique egress) refuse la
        // cible avant toute connexion : le run échoue avec le motif SSRF.
        let mut wf = trigger_only("wf-ssrf");
        wf.nodes.push(Node {
            position: None,
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

    /// Signal « l'appel HTTP a commencé » (#259). Un run réclamé n'a pas
    /// forcément commencé son premier nœud : sous charge, le thread bloquant
    /// du moteur démarre plus tard, et un drain lancé trop tôt arrête le run
    /// avant `entry`. Les tests de drain attendent donc ce signal, qui prouve
    /// qu'un nœud est en cours.
    #[derive(Clone, Default)]
    struct CallStarted(Arc<tokio::sync::Notify>);

    impl CallStarted {
        /// Appelé par le transport, sur le thread bloquant du moteur. Le
        /// permis est gardé si le test n'attend pas encore.
        fn signal(&self) {
            self.0.notify_one();
        }

        /// Attend le début de l'appel ; borne large, simple filet contre un
        /// test bloqué (ce n'est pas elle qui synchronise).
        async fn wait(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(30), self.0.notified())
                .await
                .expect("l'appel HTTP n'a jamais commencé");
        }
    }

    /// Transport qui met `delay` à répondre : un run « long ».
    #[derive(Clone)]
    struct SlowHttp {
        delay: std::time::Duration,
        started: CallStarted,
    }
    impl HttpClient for SlowHttp {
        fn send(&self, _req: &HttpRequest) -> Result<HttpResponse, String> {
            self.started.signal();
            std::thread::sleep(self.delay);
            Ok(HttpResponse {
                status: 200,
                body: json!({ "ok": true }),
            })
        }
    }

    fn http_workflow(id: &str) -> WorkflowGraph {
        let mut wf = trigger_only(id);
        wf.nodes.push(Node {
            position: None,
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

    /// Transport qui efface les runs d'`owner` pendant l'appel : le compte du
    /// propriétaire est supprimé alors que son run s'exécute (#229).
    #[derive(Clone)]
    struct ErasingHttp {
        store: Arc<InMemoryRunStateStore>,
        owner: &'static str,
    }

    impl HttpClient for ErasingHttp {
        fn send(&self, _req: &HttpRequest) -> Result<HttpResponse, String> {
            self.store.remove_owner(self.owner).unwrap();
            Ok(HttpResponse {
                status: 200,
                body: json!({ "ok": true }),
            })
        }
    }

    #[tokio::test]
    async fn a_run_erased_during_execution_is_not_recreated() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        seed_owned(&repo, &store, http_workflow("wf-e"), "run-e", Some("alice")).await;
        let http = ErasingHttp {
            store: Arc::clone(&store),
            owner: "alice",
        };
        let exec = RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http);

        let verdict = exec.handle(&delivery("run-e", "wf-e")).await;
        assert_eq!(verdict, HandlerVerdict::Ok, "acquitté, pas de rejeu");
        assert_eq!(
            store.load("run-e").await.unwrap(),
            None,
            "ni le run ni ses sorties ne sont réécrits"
        );
    }

    /// Pool d'un consommateur avec un run « long » en file, démarré et
    /// attendu jusqu'à ce que le run soit dans son appel lent.
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
        let started = CallStarted::default();
        let exec = RunExecutor::with_http(
            repo,
            Arc::clone(&store) as Arc<_>,
            resolver(),
            SlowHttp {
                delay,
                started: started.clone(),
            },
        );
        let in_flight = exec.in_flight();
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(exec);
        let idem: Arc<dyn crate::IdempotencyStore + Send + Sync> =
            Arc::new(crate::InMemoryIdempotencyStore::new());
        let running = crate::WorkerPool::new(queue, handler, idem, "runs", 1)
            .with_idle_sleep(std::time::Duration::from_millis(10))
            .start();
        started.wait().await;
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
            &autome_core::InMemoryQueue::new(),
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(outcome, crate::DrainOutcome::Completed);
        let cp = store.load("run-slow").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Succeeded);
        assert!(in_flight.snapshot().is_empty());
    }

    /// Run sans message dans l'outbox (créé avant #239) : il ne peut pas
    /// être remis en file, il est clos en échec comme avant.
    #[tokio::test]
    async fn drain_past_the_timeout_fails_the_interrupted_runs_at_once() {
        let (running, in_flight, store) =
            running_pool_with_slow_run(std::time::Duration::from_secs(3)).await;
        let started = std::time::Instant::now();
        let outcome = crate::drain(
            running,
            &in_flight,
            store.as_ref(),
            &autome_core::InMemoryQueue::new(),
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
                requeued: vec![],
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

    // ───── Reprise des runs interrompus (#239) ─────

    /// Workflow `entry → first → second`, deux appels HTTP.
    fn two_calls_workflow(id: &str, first_params: Value) -> WorkflowGraph {
        let mut wf = trigger_only(id);
        for (node, params) in [
            ("first", first_params),
            (
                "second",
                json!({ "method": "GET", "url": "https://example.com/second" }),
            ),
        ] {
            wf.nodes.push(Node {
                position: None,
                id: NodeId::new(node),
                name: node.into(),
                kind: NodeKind::Action {
                    action_type: "http_request".into(),
                },
                params,
            });
        }
        for (from, to) in [("entry", "first"), ("first", "second")] {
            wf.edges.push(Edge {
                from: NodeId::new(from),
                to: NodeId::new(to),
                from_port: None,
            });
        }
        wf
    }

    fn first_call() -> Value {
        json!({ "method": "GET", "url": "https://example.com/first" })
    }

    /// Run créé comme par le dispatcher : run `queued` + message dans l'outbox.
    async fn enqueue_run(
        repo: &Arc<InMemoryWorkflowRepository>,
        store: &Arc<InMemoryRunStateStore>,
        wf: WorkflowGraph,
        run_id: &str,
    ) {
        let wf_id = wf.id.0.clone();
        repo.create(LEGACY_NAMESPACE, wf).await.unwrap();
        let message = autome_core::OutboxMessage {
            run_id: run_id.into(),
            queue: "runs".into(),
            generation: 0,
            message: delivery(run_id, &wf_id).message,
        };
        store
            .enqueue(
                &RunCheckpoint::queued(run_id, &wf_id, 1, "manual", NOW),
                &message,
            )
            .await
            .unwrap();
        store.mark_published(run_id, 0, NOW).await.unwrap();
    }

    /// Transport qui enregistre les URL appelées, lent sur `/first` (dont
    /// il signale le début).
    #[derive(Clone, Default)]
    struct CountingHttp {
        urls: Arc<Mutex<Vec<String>>>,
        first_delay: std::time::Duration,
        first_started: CallStarted,
    }

    impl HttpClient for CountingHttp {
        fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
            self.urls.lock().unwrap().push(req.url.clone());
            if req.url.ends_with("/first") {
                self.first_started.signal();
                std::thread::sleep(self.first_delay);
            }
            Ok(HttpResponse {
                status: 200,
                body: json!({ "from": req.url }),
            })
        }
    }

    impl CountingHttp {
        fn calls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
    }

    /// Simule un worker tué pendant `node` : tentative 1 réclamée, nœuds
    /// précédents enregistrés, `node` marqué en cours, puis remise en file
    /// (battement expiré).
    async fn killed_during(store: &InMemoryRunStateStore, dag: &Dag, run_id: &str, node: &str) {
        store.try_claim(run_id, NOW).await.unwrap().unwrap();
        // Point de reprise réel : le moteur arrêté juste avant `node`.
        struct Before(&'static str);
        impl RunObserver for Before {
            fn before_node(&self, n: &NodeId, _: &Cursor) -> Control {
                if n.0 == self.0 {
                    Control::Stop
                } else {
                    Control::Continue
                }
            }
            fn after_node(&self, _: &Cursor) {}
        }
        let node: &'static str = Box::leak(node.to_string().into_boxed_str());
        let runner = |_: &Node, _: &RunContext| Ok(json!({ "before": "crash" }));
        let Ok(Execution::Interrupted { cursor, .. }) =
            resume_sequential(dag, &runner, Cursor::start(dag), &Before(node))
        else {
            panic!("interruption attendue");
        };
        let done = RunProgress::Done {
            resume_state: serde_json::to_value(&cursor).unwrap(),
        };
        assert!(store.record_progress(run_id, 1, &done, NOW).await.unwrap());
        let starting = RunProgress::Starting { node: node.into() };
        assert!(store
            .record_progress(run_id, 1, &starting, NOW)
            .await
            .unwrap());
        store.requeue(run_id, 1, NOW + 1).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_successful_run_leaves_no_resume_state_nor_message() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        enqueue_run(
            &repo,
            &store,
            two_calls_workflow("wf-ok", first_call()),
            "run-ok",
        )
        .await;
        let http = CountingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());
        assert_eq!(
            exec.handle(&delivery("run-ok", "wf-ok")).await,
            HandlerVerdict::Ok
        );
        let cp = store.load("run-ok").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Succeeded);
        assert_eq!(cp.attempt, 1);
        assert_eq!(cp.next_node, None);
        assert_eq!(cp.resume_state, None);
        assert_eq!(cp.outputs.len(), 3);
        assert_eq!(http.calls().len(), 2);
        assert!(store.requeue("run-ok", 1, NOW).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_stopping_worker_requeues_the_run_between_two_nodes_and_another_resumes_it() {
        use autome_core::{InMemoryQueue, Queue};
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        enqueue_run(
            &repo,
            &store,
            two_calls_workflow("wf-s", first_call()),
            "run-s",
        )
        .await;
        let queue = Arc::new(InMemoryQueue::new());
        queue
            .publish("runs", delivery("run-s", "wf-s").message)
            .await
            .unwrap();

        // Worker A : arrêté pendant `first` (300 ms), délai de drain large.
        let http_a = CountingHttp {
            first_delay: std::time::Duration::from_millis(300),
            ..Default::default()
        };
        let exec_a = RunExecutor::with_http(
            Arc::clone(&repo) as Arc<_>,
            Arc::clone(&store) as Arc<_>,
            resolver(),
            http_a.clone(),
        )
        .with_queue(Arc::clone(&queue) as Arc<_>);
        let in_flight = exec_a.in_flight();
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(exec_a);
        let running = crate::WorkerPool::new(
            Arc::clone(&queue) as Arc<_>,
            handler,
            Arc::new(crate::InMemoryIdempotencyStore::new()),
            "runs",
            1,
        )
        .with_idle_sleep(std::time::Duration::from_millis(10))
        .start();
        // Drain pendant `first`, pas seulement une fois le run réclamé.
        http_a.first_started.wait().await;
        let outcome = crate::drain(
            running,
            &in_flight,
            store.as_ref(),
            queue.as_ref(),
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(outcome, crate::DrainOutcome::Completed);
        // `first` terminé, `second` jamais lancé par A.
        assert_eq!(http_a.calls(), vec!["https://example.com/first"]);
        let cp = store.load("run-s").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Queued, "remis en file");
        assert_eq!(cp.next_node, None, "aucun nœud en cours");
        assert!(cp.resume_state.is_some());

        // Le message republié porte une nouvelle clé d'idempotence.
        let redelivered = queue.poll("runs").await.unwrap().expect("republié");
        assert_eq!(
            redelivered
                .message
                .headers
                .get("idempotency-key")
                .map(String::as_str),
            Some("run-s#1")
        );

        // Worker B : reprend après `first`, sans le rejouer.
        let http_b = CountingHttp::default();
        let exec_b = RunExecutor::with_http(
            repo,
            Arc::clone(&store) as Arc<_>,
            resolver(),
            http_b.clone(),
        );
        assert_eq!(exec_b.handle(&redelivered).await, HandlerVerdict::Ok);
        assert_eq!(http_b.calls(), vec!["https://example.com/second"]);
        let cp = store.load("run-s").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Succeeded);
        assert_eq!(cp.attempt, 2);
        assert_eq!(
            cp.outputs["first"]["body"],
            json!({ "from": "https://example.com/first" })
        );
        assert_eq!(
            cp.outputs["second"]["body"],
            json!({ "from": "https://example.com/second" })
        );
        assert!(cp.outputs.contains_key("entry"));
    }

    #[tokio::test]
    async fn a_drain_timeout_requeues_the_run_and_its_uncertain_node_is_not_replayed() {
        use autome_core::{InMemoryQueue, Queue};
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        enqueue_run(
            &repo,
            &store,
            two_calls_workflow("wf-t", first_call()),
            "run-t",
        )
        .await;
        let queue = Arc::new(InMemoryQueue::new());
        queue
            .publish("runs", delivery("run-t", "wf-t").message)
            .await
            .unwrap();
        let http_a = CountingHttp {
            first_delay: std::time::Duration::from_secs(3),
            ..Default::default()
        };
        let exec_a = RunExecutor::with_http(
            Arc::clone(&repo) as Arc<_>,
            Arc::clone(&store) as Arc<_>,
            resolver(),
            http_a.clone(),
        );
        let in_flight = exec_a.in_flight();
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(exec_a);
        let running = crate::WorkerPool::new(
            Arc::clone(&queue) as Arc<_>,
            handler,
            Arc::new(crate::InMemoryIdempotencyStore::new()),
            "runs",
            1,
        )
        .with_idle_sleep(std::time::Duration::from_millis(10))
        .start();
        // Attendre que l'appel de `first` soit en cours (marqué en cours
        // avant l'appel).
        http_a.first_started.wait().await;
        let outcome = crate::drain(
            running,
            &in_flight,
            store.as_ref(),
            queue.as_ref(),
            std::time::Duration::from_millis(200),
        )
        .await;
        assert_eq!(
            outcome,
            crate::DrainOutcome::Interrupted {
                requeued: vec!["run-t".into()],
                failed: vec![]
            }
        );
        let cp = store.load("run-t").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Queued);
        assert_eq!(cp.next_node.as_deref(), Some("first"));

        // Le worker suivant ne rejoue pas `first` (effet incertain).
        let redelivered = queue.poll("runs").await.unwrap().expect("republié");
        let http_b = CountingHttp::default();
        let exec_b = RunExecutor::with_http(
            repo,
            Arc::clone(&store) as Arc<_>,
            resolver(),
            http_b.clone(),
        );
        assert_eq!(
            exec_b.handle(&redelivered).await,
            HandlerVerdict::DeadLetter
        );
        assert!(http_b.calls().is_empty(), "rien n'est rejoué");
        let cp = store.load("run-t").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        let error = cp.error.clone().unwrap();
        assert!(
            error.contains("`first`") && error.contains("incertain"),
            "{error}"
        );
        assert!(
            cp.outputs.contains_key("entry"),
            "sorties déjà produites gardées : {cp:?}"
        );
    }

    #[tokio::test]
    async fn a_node_allowed_to_retry_is_replayed_after_a_crash() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        let retryable = json!({
            "method": "GET", "url": "https://example.com/first",
            "_error": { "max_attempts": 2 }
        });
        let wf = two_calls_workflow("wf-r", retryable);
        let dag = Dag::from_workflow(&wf).unwrap();
        enqueue_run(&repo, &store, wf, "run-r").await;
        killed_during(&store, &dag, "run-r", "first").await;

        let http = CountingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());
        assert_eq!(
            exec.handle(&delivery("run-r", "wf-r")).await,
            HandlerVerdict::Ok
        );
        assert_eq!(
            http.calls(),
            vec!["https://example.com/first", "https://example.com/second"]
        );
        let cp = store.load("run-r").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Succeeded);
        assert_eq!(cp.attempt, 2);
        // Le trigger n'a pas été réexécuté : sa sortie est celle d'avant.
        assert_eq!(cp.outputs["entry"], json!({ "before": "crash" }));
    }

    #[tokio::test]
    async fn a_crash_during_a_side_effect_free_node_resumes_on_it() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        let wf = two_calls_workflow("wf-c", first_call());
        let dag = Dag::from_workflow(&wf).unwrap();
        enqueue_run(&repo, &store, wf, "run-c").await;
        // Tué pendant le trigger : rien d'incertain, tout est exécuté.
        killed_during(&store, &dag, "run-c", "entry").await;
        let http = CountingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());
        assert_eq!(
            exec.handle(&delivery("run-c", "wf-c")).await,
            HandlerVerdict::Ok
        );
        assert_eq!(http.calls().len(), 2);
    }

    #[tokio::test]
    async fn a_workflow_modified_since_the_interruption_is_not_resumed() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        let wf = two_calls_workflow("wf-m", first_call());
        let dag = Dag::from_workflow(&wf).unwrap();
        enqueue_run(&repo, &store, wf.clone(), "run-m").await;
        killed_during(&store, &dag, "run-m", "second").await;
        let mut changed = wf;
        changed.nodes[2].params = json!({ "method": "POST", "url": "https://example.com/other" });
        repo.update(LEGACY_NAMESPACE, changed).await.unwrap();

        let http = CountingHttp::default();
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());
        assert_eq!(
            exec.handle(&delivery("run-m", "wf-m")).await,
            HandlerVerdict::DeadLetter
        );
        assert!(http.calls().is_empty());
        let cp = store.load("run-m").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        assert!(cp.error.unwrap().contains("workflow modifié"));
    }

    #[tokio::test]
    async fn a_run_interrupted_too_often_is_abandoned() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        enqueue_run(&repo, &store, trigger_only("wf-x"), "run-x").await;
        for attempt in 1..=MAX_RUN_ATTEMPTS {
            store.try_claim("run-x", NOW).await.unwrap().unwrap();
            store.requeue("run-x", attempt, NOW).await.unwrap().unwrap();
        }
        let exec = RunExecutor::with_http(
            repo,
            Arc::clone(&store) as Arc<_>,
            resolver(),
            CountingHttp::default(),
        );
        assert_eq!(
            exec.handle(&delivery("run-x", "wf-x")).await,
            HandlerVerdict::DeadLetter
        );
        let cp = store.load("run-x").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Failed);
        assert!(cp.error.unwrap().contains("tentatives"));
    }

    /// Transport qui, pendant `/first`, simule la reprise du run par un autre
    /// worker (battement expiré) : remise en file puis seconde prise.
    #[derive(Clone)]
    struct TakenOverHttp {
        store: Arc<InMemoryRunStateStore>,
        inner: CountingHttp,
    }

    impl HttpClient for TakenOverHttp {
        fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
            if req.url.ends_with("/first") {
                let store = Arc::clone(&self.store);
                std::thread::spawn(move || {
                    tokio::runtime::Runtime::new().unwrap().block_on(async {
                        store.requeue("run-z", 1, NOW).await.unwrap().unwrap();
                        store.try_claim("run-z", NOW).await.unwrap().unwrap();
                    })
                })
                .join()
                .unwrap();
            }
            self.inner.send(req)
        }
    }

    #[tokio::test]
    async fn a_worker_that_lost_its_run_stops_before_the_next_node_and_writes_nothing() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        enqueue_run(
            &repo,
            &store,
            two_calls_workflow("wf-z", first_call()),
            "run-z",
        )
        .await;
        let http = TakenOverHttp {
            store: Arc::clone(&store),
            inner: CountingHttp::default(),
        };
        let exec =
            RunExecutor::with_http(repo, Arc::clone(&store) as Arc<_>, resolver(), http.clone());
        assert_eq!(
            exec.handle(&delivery("run-z", "wf-z")).await,
            HandlerVerdict::Ok
        );
        assert_eq!(
            http.inner.calls(),
            vec!["https://example.com/first"],
            "`second` non lancé"
        );
        let cp = store.load("run-z").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Running, "laissé à la tentative 2");
        assert_eq!(cp.attempt, 2);
        // Toujours le nœud que la tentative 1 avait commencé : la tentative 2
        // tranchera à sa reprise.
        assert_eq!(cp.next_node.as_deref(), Some("first"));
    }
}

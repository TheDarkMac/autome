//! Worker pool (F5.2).
//!
//! Consomme une [`autome_core::Queue`] (F5.1), passe chaque livraison à un
//! [`Handler`], et applique le verdict (`ack` / `nack(Requeue)` /
//! `nack(DeadLetter)`).
//!
//! L'implémentation est **asynchrone** (tâches tokio + `Arc<AtomicBool>` pour
//! le shutdown) depuis la bascule async des traits (dette #107) : le pool
//! `await` directement la [`Queue`] et le [`Handler`] async, ce qui permet de
//! brancher un broker réel (`lapin`/AMQP) sans couche d'adaptation.
//!
//! ## Architecture
//!
//! - [`Handler`] : trait que l'appelant implémente pour traiter un message.
//!   Une impl blanket existe pour toute closure `Fn(&Delivery) -> HandlerVerdict`.
//! - [`HandlerVerdict`] : résultat (`Ok` / `Retry` / `DeadLetter`) que le
//!   pool traduit en `ack` ou `nack`.
//! - [`IdempotencyStore`] (F5.2.c) : déduplication par `idempotency-key`
//!   présent dans les en-têtes du [`Message`], **locale au process** (fast
//!   path). L'impl `InMemoryIdempotencyStore` garde un `HashSet` sous `Mutex`.
//!   La garantie *entre répliques* vient du claim atomique du run en base
//!   (`RunStateStore::try_claim`, cf. [`RunExecutor`]).
//! - [`WorkerPool::process_one`] : unité atomique (poll + handle + ack/nack),
//!   testable sans pool.
//! - [`WorkerPool::start`] : spawne `concurrency` tâches tokio qui attendent
//!   le message suivant par [`Queue::receive`] (consommateur poussé par le
//!   broker en RabbitMQ, #237) et le traitent comme `process_one`. Renvoie un
//!   [`RunningPool`] dont `stop()` signale l'arrêt, rend au broker les
//!   messages reçus d'avance ([`Queue::stop_receiving`]) et `await`
//!   proprement les tâches.
//!
//! ## Concurrence & backpressure (F5.5)
//!
//! - `concurrency` (paramètre de [`WorkerPool::new`]) est le **cap
//!   d'exécution concurrente par queue** : N threads consomment cette queue
//!   en parallèle. Augmenter `concurrency` augmente le débit jusqu'à
//!   saturation du handler (CPU local, latence externe, etc.).
//! - La **backpressure côté producteur** est portée par
//!   [`autome_core::BoundedQueue`] (F5.5) : le scheduler/API se voit
//!   refuser une publication par `Error::Backpressure` quand la queue est
//!   pleine, et peut alors retenter / dégrader gracieusement plutôt que
//!   de faire enfler la mémoire.
//! - Limites par workflow (sémaphore inter-runs d'une même définition) et
//!   rate limits par tenant relèveront de F6.5 (RBAC) — non couverts ici.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use autome_core::{Delivery, Nack, Queue, Result};
use tokio::task::JoinHandle;

use crate::metrics::DeliveryOutcome;

pub mod dispatch;
pub mod executor;
pub mod liveness;
pub mod metrics;
pub mod outbox;
pub mod probe;
pub mod schema;
pub mod shutdown;
pub mod triggers;
pub use dispatch::{
    ConnectorCredentialsMode, CredentialResolver, EnvCredentialResolver, OwnerCredentialResolver,
};
pub use executor::{reap_orphans, RunExecutor};
pub use liveness::PoolLiveness;
pub use metrics::{RequeueReason, WorkerMetrics};
pub use shutdown::{drain, DrainOutcome, InFlightRuns};
pub use triggers::TriggerLoop;

/// Verdict que retourne un [`Handler`] après traitement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerVerdict {
    /// Succès — le pool fera `ack`.
    Ok,
    /// Échec retryable — le pool fera `nack(Requeue)`.
    Retry,
    /// Échec définitif — le pool fera `nack(DeadLetter)`.
    DeadLetter,
}

/// Logique métier d'un worker : traite une livraison et retourne son verdict.
///
/// **Async** (dette #107) : le handler accède aux stores/connecteurs async.
#[async_trait]
pub trait Handler: Send + Sync {
    /// Traite `delivery` et retourne le verdict (ack/retry/dlq).
    async fn handle(&self, delivery: &Delivery) -> HandlerVerdict;
}

/// Tout `Fn(&Delivery) -> HandlerVerdict` synchrone est un `Handler`.
#[async_trait]
impl<F> Handler for F
where
    F: Fn(&Delivery) -> HandlerVerdict + Send + Sync,
{
    async fn handle(&self, delivery: &Delivery) -> HandlerVerdict {
        (self)(delivery)
    }
}

/// Déduplication des messages déjà traités (F5.2.c).
///
/// Le worker pool consulte le store pour la clé `idempotency-key` (si
/// présente dans les en-têtes du [`autome_core::Message`]). Si la clé est
/// déjà traitée, le message est `ack`'é sans appeler le handler.
///
/// La clé n'est marquée qu'après un verdict **définitif** (`Ok` /
/// `DeadLetter`) : un message renvoyé en `Retry` revient avec la même clé et
/// doit être retraité (#174).
pub trait IdempotencyStore {
    /// `true` si `key` a déjà été traitée définitivement (handler à *skip*).
    fn is_done(&self, key: &str) -> bool;

    /// Marque `key` comme traitée définitivement.
    fn mark_done(&self, key: &str);
}

/// Store de déduplication in-process (`HashSet` sous `Mutex`).
///
/// Fast path local : évite de recharger un run déjà traité par ce process.
/// Ne protège pas entre répliques — c'est le rôle du claim en base.
#[derive(Debug, Default)]
pub struct InMemoryIdempotencyStore {
    seen: Mutex<HashSet<String>>,
}

impl InMemoryIdempotencyStore {
    /// Construit un store vide.
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdempotencyStore for InMemoryIdempotencyStore {
    fn is_done(&self, key: &str) -> bool {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(key)
    }

    fn mark_done(&self, key: &str) {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.to_string());
    }
}

/// Résultat d'un `process_one`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// File vide à ce moment-là — rien n'a été fait.
    Idle,
    /// Message déjà traité (clé d'idempotence) — `ack` sans appel du handler.
    Deduplicated,
    /// Handler appelé, verdict suivi.
    Handled(HandlerVerdict),
}

/// Pool de workers consommateurs d'une queue.
///
/// Construit avec des `Arc<dyn …>` plutôt que des génériques pour garder
/// l'API simple côté appelant (binaires, tests) — le coût de la vtable
/// est négligeable comparé à un poll réseau.
pub struct WorkerPool {
    queue: Arc<dyn Queue + Send + Sync>,
    handler: Arc<dyn Handler + Send + Sync>,
    idempotency: Arc<dyn IdempotencyStore + Send + Sync>,
    queue_name: String,
    concurrency: usize,
    poll_idle_sleep: Duration,
    /// Battements des consommateurs, lus par la sonde de liveness (#192).
    liveness: Arc<PoolLiveness>,
    /// Sort des livraisons et erreurs de file (#235).
    metrics: Arc<WorkerMetrics>,
}

impl WorkerPool {
    /// Construit un pool.
    ///
    /// `concurrency` est plafonné à `1` au minimum.
    pub fn new(
        queue: Arc<dyn Queue + Send + Sync>,
        handler: Arc<dyn Handler + Send + Sync>,
        idempotency: Arc<dyn IdempotencyStore + Send + Sync>,
        queue_name: impl Into<String>,
        concurrency: usize,
    ) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            queue,
            handler,
            idempotency,
            queue_name: queue_name.into(),
            concurrency,
            poll_idle_sleep: Duration::from_millis(50),
            liveness: Arc::new(PoolLiveness::new(concurrency, liveness::now_ms())),
            metrics: Arc::new(WorkerMetrics::new()),
        }
    }

    /// Builder : registre où compter les livraisons et les erreurs de file
    /// (#235) ; par défaut un registre propre au pool.
    pub fn with_metrics(mut self, metrics: Arc<WorkerMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Battements des consommateurs (à passer à la sonde de liveness).
    pub fn liveness(&self) -> Arc<PoolLiveness> {
        Arc::clone(&self.liveness)
    }

    /// Builder : attente d'un message par tour de boucle quand la file est
    /// vide (délai de [`Queue::receive`], complété par une pause si la file
    /// rend la main plus tôt) ; borne aussi la réaction au signal d'arrêt.
    pub fn with_idle_sleep(mut self, dur: Duration) -> Self {
        self.poll_idle_sleep = dur;
        self
    }

    /// Traite UN message si disponible. Exposée pour les tests et pour piloter
    /// le worker en mode déterministe.
    pub async fn process_one(&self) -> Result<ProcessOutcome> {
        let delivery = self.queue.poll(&self.queue_name).await?;
        self.process(delivery, None).await
    }

    /// Traite `delivery` (sinon [`ProcessOutcome::Idle`]) pour le
    /// consommateur `consumer`, marqué occupé pendant le traitement du
    /// message (liveness, #192).
    async fn process(
        &self,
        delivery: Option<Delivery>,
        consumer: Option<usize>,
    ) -> Result<ProcessOutcome> {
        let Some(delivery) = delivery else {
            return Ok(ProcessOutcome::Idle);
        };
        // Idempotence (F5.2.c) : si le message porte une clé déjà traitée,
        // ack sans appeler le handler.
        let key = delivery.message.headers.get("idempotency-key").cloned();
        if let Some(key) = &key {
            if self.idempotency.is_done(key) {
                self.queue.ack(&delivery.id).await?;
                self.metrics.delivery(DeliveryOutcome::Deduplicated);
                return Ok(ProcessOutcome::Deduplicated);
            }
        }
        if let Some(id) = consumer {
            self.liveness.busy(id, liveness::now_ms());
        }
        let verdict = self.handler.handle(&delivery).await;
        if let Some(id) = consumer {
            self.liveness.idle(id, liveness::now_ms());
        }
        match &verdict {
            HandlerVerdict::Ok => self.queue.ack(&delivery.id).await?,
            HandlerVerdict::Retry => self.queue.nack(&delivery.id, Nack::Requeue).await?,
            HandlerVerdict::DeadLetter => self.queue.nack(&delivery.id, Nack::DeadLetter).await?,
        }
        self.metrics.delivery(DeliveryOutcome::from(&verdict));
        // Marquée seulement une fois le sort du message fixé : un `Retry`
        // doit pouvoir être retraité à la redélivrance.
        if let (Some(key), false) = (&key, verdict == HandlerVerdict::Retry) {
            self.idempotency.mark_done(key);
        }
        Ok(ProcessOutcome::Handled(verdict))
    }

    /// Démarre `self.concurrency` **tâches tokio** consommatrices. Renvoie un
    /// [`RunningPool`] pour stopper proprement (signal d'arrêt + join). À
    /// appeler depuis un runtime tokio.
    pub fn start(self) -> RunningPool {
        let shutdown = Arc::new(AtomicBool::new(false));
        let pool = Arc::new(self);
        let mut handles = Vec::with_capacity(pool.concurrency);
        for i in 0..pool.concurrency {
            let pool = Arc::clone(&pool);
            let stop = Arc::clone(&shutdown);
            handles.push(tokio::spawn(async move {
                worker_loop(&pool, &stop, i).await;
            }));
        }
        RunningPool {
            handles,
            shutdown,
            queue: Arc::clone(&pool.queue),
            queue_name: pool.queue_name.clone(),
        }
    }
}

/// Plafond de la pause entre deux tentatives après des erreurs de file.
const MAX_ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Pause après `failures` erreurs de file consécutives : `base` doublé à
/// chaque échec, plafonné à [`MAX_ERROR_BACKOFF`]. Évite de marteler un broker
/// indisponible (et les logs) toutes les `base` ms pendant une panne (#180).
fn error_backoff(base: Duration, failures: u32) -> Duration {
    let factor = 1u32 << failures.saturating_sub(1).min(16);
    base.saturating_mul(factor).min(MAX_ERROR_BACKOFF)
}

async fn worker_loop(pool: &WorkerPool, shutdown: &AtomicBool, id: usize) {
    let mut failures = 0u32;
    while !shutdown.load(Ordering::Acquire) {
        pool.liveness.tick(id, liveness::now_ms());
        let started = tokio::time::Instant::now();
        let outcome = match pool
            .queue
            .receive(&pool.queue_name, pool.poll_idle_sleep)
            .await
        {
            Ok(delivery) => pool.process(delivery, Some(id)).await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(outcome) => {
                if failures > 0 {
                    tracing::info!(consumer = id, failures, "file de nouveau disponible");
                    failures = 0;
                }
                // File vide : patienter le reste du délai si elle a rendu la
                // main plus tôt (file en mémoire, sans attente).
                if outcome == ProcessOutcome::Idle {
                    let left = pool.poll_idle_sleep.saturating_sub(started.elapsed());
                    if !left.is_zero() {
                        tokio::time::sleep(left).await;
                    }
                }
            }
            Err(e) => {
                pool.metrics.queue_error();
                failures = failures.saturating_add(1);
                let pause = error_backoff(pool.poll_idle_sleep, failures);
                tracing::error!(
                    consumer = id,
                    failures,
                    retry_in_ms = pause.as_millis() as u64,
                    error = %e,
                    "erreur de file"
                );
                tokio::time::sleep(pause).await;
            }
        }
    }
}

/// Handle d'un pool en cours d'exécution. Doit être stoppé proprement par
/// [`RunningPool::stop`] (sinon `Drop` log un warning, mais ne join pas —
/// les threads continueraient en arrière-plan jusqu'à arrêt du process).
#[must_use = "RunningPool doit être stoppé via `.stop().await` pour arrêter proprement les threads"]
pub struct RunningPool {
    handles: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    queue: Arc<dyn Queue + Send + Sync>,
    queue_name: String,
}

impl RunningPool {
    /// Signale l'arrêt aux tâches puis rend au broker les messages reçus
    /// d'avance (#237) : les autres répliques les prennent sans attendre la
    /// fin du drain ni la fermeture de la connexion.
    async fn halt(&self) {
        self.shutdown.store(true, Ordering::Release);
        if let Err(e) = self.queue.stop_receiving(&self.queue_name).await {
            tracing::warn!(
                error = %e,
                "arrêt de la consommation incomplet : les messages reçus d'avance \
                 reviendront en file à la fermeture de la connexion"
            );
        }
    }

    /// Signale l'arrêt et `await` la fin de toutes les tâches. Consommante :
    /// après cet appel le pool n'est plus utilisable. À appeler dans un runtime.
    pub async fn stop(mut self) {
        self.halt().await;
        // Vider `handles` AVANT que `Drop` voie un Vec non-vide (sinon abort
        // à tort des tâches déjà attendues).
        let handles = std::mem::take(&mut self.handles);
        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Signale l'arrêt puis attend la fin des tâches pendant au plus
    /// `timeout` (#200). `true` si toutes ont terminé ; sinon elles tournent
    /// encore, à interrompre par [`abort`](Self::abort).
    pub async fn stop_within(&mut self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        self.halt().await;
        let all = async {
            for handle in self.handles.iter_mut() {
                let _ = handle.await;
            }
        };
        let done = tokio::time::timeout_at(deadline, all).await.is_ok();
        if done {
            self.handles.clear();
        }
        done
    }

    /// Interrompt les tâches encore en cours (après un
    /// [`stop_within`](Self::stop_within) dépassé).
    pub fn abort(mut self) {
        self.shutdown.store(true, Ordering::Release);
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }

    /// Nombre de tâches vivantes (utile pour les assertions de tests).
    pub fn task_count(&self) -> usize {
        self.handles.len()
    }
}

impl Drop for RunningPool {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            // L'appelant a oublié `stop()` : on signale l'arrêt et on **abort**
            // les tâches (on ne peut pas `await` dans `Drop`).
            self.shutdown.store(true, Ordering::Release);
            for handle in &self.handles {
                handle.abort();
            }
            tracing::warn!(
                tasks = self.handles.len(),
                "RunningPool abandonné sans stop() : tâches avortées"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{DeliveryId, InMemoryQueue, Message};
    use serde_json::json;

    fn build(
        queue: Arc<InMemoryQueue>,
        verdict: HandlerVerdict,
    ) -> (WorkerPool, Arc<dyn IdempotencyStore + Send + Sync>) {
        let handler: Arc<dyn Handler + Send + Sync> =
            Arc::new(move |_d: &Delivery| verdict.clone());
        let idem: Arc<dyn IdempotencyStore + Send + Sync> =
            Arc::new(InMemoryIdempotencyStore::new());
        let q_dyn: Arc<dyn Queue + Send + Sync> = queue;
        let pool = WorkerPool::new(q_dyn, handler, Arc::clone(&idem), "runs", 1);
        (pool, idem)
    }

    // ───── #192 — liveness des consommateurs ─────

    /// Handler qui « exécute un run » pendant la durée donnée.
    struct SlowHandler(Duration);

    #[async_trait]
    impl Handler for SlowHandler {
        async fn handle(&self, _d: &Delivery) -> HandlerVerdict {
            tokio::time::sleep(self.0).await;
            HandlerVerdict::Ok
        }
    }

    #[tokio::test]
    async fn a_long_run_keeps_the_pool_alive_and_idle_loops_keep_beating() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({ "n": 1 })))
            .await
            .unwrap();
        let handler: Arc<dyn Handler + Send + Sync> =
            Arc::new(SlowHandler(Duration::from_millis(400)));
        let idem: Arc<dyn IdempotencyStore + Send + Sync> =
            Arc::new(InMemoryIdempotencyStore::new());
        let pool = WorkerPool::new(q.clone(), handler, idem, "runs", 2)
            .with_idle_sleep(Duration::from_millis(10));
        let live = pool.liveness();
        let running = pool.start();

        // Pendant le « run » (400 ms) : seuil de boucle de 100 ms dépassé pour
        // le consommateur occupé, mais il est occupé depuis moins que la limite.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(live.check(liveness::now_ms(), 100, 10_000).is_ok());
        // Une limite de run plus courte que le traitement le signale figé.
        assert!(live.check(liveness::now_ms(), 100, 100).is_err());

        // Run terminé, file vide : les deux consommateurs bouclent et battent.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(live.check(liveness::now_ms(), 100, 200).is_ok());

        running.stop().await;
        // Pool arrêté : plus personne ne bat, la liveness finit par échouer.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(live.check(liveness::now_ms(), 100, 200).is_err());
    }

    // ───── F5.2.a — process_one ─────

    // ───── #235 — métriques ─────

    /// File dont chaque opération échoue (broker injoignable).
    struct DownQueue;

    #[async_trait]
    impl Queue for DownQueue {
        async fn publish(&self, _name: &str, _msg: Message) -> Result<()> {
            Err(autome_core::Error::Invalid("broker down".into()))
        }
        async fn poll(&self, _name: &str) -> Result<Option<Delivery>> {
            Err(autome_core::Error::Invalid("broker down".into()))
        }
        async fn ack(&self, _id: &DeliveryId) -> Result<()> {
            Err(autome_core::Error::Invalid("broker down".into()))
        }
        async fn nack(&self, _id: &DeliveryId, _verdict: Nack) -> Result<()> {
            Err(autome_core::Error::Invalid("broker down".into()))
        }
        async fn depth(&self, _name: &str) -> Result<usize> {
            Err(autome_core::Error::Invalid("broker down".into()))
        }
    }

    #[tokio::test]
    async fn deliveries_are_counted_by_outcome() {
        let q = Arc::new(InMemoryQueue::new());
        for _ in 0..2 {
            let msg = Message::new(json!({})).with_header("idempotency-key", "k");
            q.publish("runs", msg).await.unwrap();
        }
        q.publish("runs", Message::new(json!({}))).await.unwrap();
        let metrics = Arc::new(WorkerMetrics::new());
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::DeadLetter);
        let pool = pool.with_metrics(Arc::clone(&metrics));
        for _ in 0..3 {
            pool.process_one().await.unwrap();
        }
        assert_eq!(pool.process_one().await.unwrap(), ProcessOutcome::Idle);

        let out = metrics.render(metrics::Gauges::default());
        assert!(out.contains("autome_worker_deliveries_total{outcome=\"dead_letter\"} 2\n"));
        assert!(out.contains("autome_worker_deliveries_total{outcome=\"deduplicated\"} 1\n"));
        assert!(out.contains("autome_worker_deliveries_total{outcome=\"ack\"} 0\n"));
        assert!(out.contains("autome_worker_queue_errors_total 0\n"));
    }

    #[tokio::test]
    async fn queue_errors_are_counted() {
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(|_d: &Delivery| HandlerVerdict::Ok);
        let idem: Arc<dyn IdempotencyStore + Send + Sync> =
            Arc::new(InMemoryIdempotencyStore::new());
        let metrics = Arc::new(WorkerMetrics::new());
        let pool = WorkerPool::new(Arc::new(DownQueue), handler, idem, "runs", 1)
            .with_idle_sleep(Duration::from_millis(1))
            .with_metrics(Arc::clone(&metrics));
        let running = pool.start();
        tokio::time::sleep(Duration::from_millis(50)).await;
        running.stop().await;

        let out = metrics.render(metrics::Gauges::default());
        let errors: u64 = out
            .lines()
            .find_map(|l| l.strip_prefix("autome_worker_queue_errors_total "))
            .unwrap()
            .parse()
            .unwrap();
        assert!(errors >= 2, "{errors}");
    }

    #[tokio::test]
    async fn process_one_empty_queue_is_idle() {
        let q = Arc::new(InMemoryQueue::new());
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::Ok);
        assert_eq!(pool.process_one().await.unwrap(), ProcessOutcome::Idle);
    }

    #[tokio::test]
    async fn process_one_ok_acks_and_drains_queue() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({ "n": 1 })))
            .await
            .unwrap();
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::Ok);
        let outcome = pool.process_one().await.unwrap();
        assert_eq!(outcome, ProcessOutcome::Handled(HandlerVerdict::Ok));
        // Queue principale vide, DLQ aussi (rien d'in-flight non plus puisque ack)
        assert_eq!(q.depth("runs").await.unwrap(), 0);
        assert_eq!(q.depth("runs.dlq").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn process_one_retry_requeues_and_increments_redeliveries() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({}))).await.unwrap();
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::Retry);
        let outcome = pool.process_one().await.unwrap();
        assert_eq!(outcome, ProcessOutcome::Handled(HandlerVerdict::Retry));
        // Le message est de retour
        assert_eq!(q.depth("runs").await.unwrap(), 1);
        let d = q.poll("runs").await.unwrap().unwrap();
        assert_eq!(d.redeliveries, 1);
    }

    #[tokio::test]
    async fn process_one_dead_letter_moves_to_dlq() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({ "n": 9 })))
            .await
            .unwrap();
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::DeadLetter);
        let outcome = pool.process_one().await.unwrap();
        assert_eq!(outcome, ProcessOutcome::Handled(HandlerVerdict::DeadLetter));
        assert_eq!(q.depth("runs").await.unwrap(), 0);
        assert_eq!(q.depth("runs.dlq").await.unwrap(), 1);
    }

    // ───── #180 — backoff sur erreurs de file ─────

    #[test]
    fn error_backoff_doubles_up_to_the_cap() {
        let base = Duration::from_millis(50);
        assert_eq!(error_backoff(base, 1), Duration::from_millis(50));
        assert_eq!(error_backoff(base, 2), Duration::from_millis(100));
        assert_eq!(error_backoff(base, 4), Duration::from_millis(400));
        assert_eq!(error_backoff(base, 8), MAX_ERROR_BACKOFF);
        assert_eq!(error_backoff(base, u32::MAX), MAX_ERROR_BACKOFF);
    }

    // ───── #237 — consommation continue ─────

    use std::sync::atomic::AtomicUsize;

    /// File en mémoire qui compte ses appels ; son `receive` rend la main
    /// sans attendre, comme le défaut du trait.
    #[derive(Default)]
    struct CountingQueue {
        inner: InMemoryQueue,
        polls: AtomicUsize,
        receives: AtomicUsize,
        stopped: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Queue for CountingQueue {
        async fn publish(&self, name: &str, msg: Message) -> Result<()> {
            self.inner.publish(name, msg).await
        }
        async fn poll(&self, name: &str) -> Result<Option<Delivery>> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            self.inner.poll(name).await
        }
        async fn receive(&self, name: &str, _wait: Duration) -> Result<Option<Delivery>> {
            self.receives.fetch_add(1, Ordering::Relaxed);
            self.inner.poll(name).await
        }
        async fn stop_receiving(&self, name: &str) -> Result<()> {
            self.stopped.lock().unwrap().push(name.to_string());
            Ok(())
        }
        async fn ack(&self, id: &DeliveryId) -> Result<()> {
            self.inner.ack(id).await
        }
        async fn nack(&self, id: &DeliveryId, verdict: Nack) -> Result<()> {
            self.inner.nack(id, verdict).await
        }
        async fn depth(&self, name: &str) -> Result<usize> {
            self.inner.depth(name).await
        }
    }

    fn counting_pool(q: &Arc<CountingQueue>, handled: &Arc<AtomicUsize>) -> WorkerPool {
        let handled = Arc::clone(handled);
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(move |_d: &Delivery| {
            handled.fetch_add(1, Ordering::Relaxed);
            HandlerVerdict::Ok
        });
        let idem: Arc<dyn IdempotencyStore + Send + Sync> =
            Arc::new(InMemoryIdempotencyStore::new());
        let q_dyn: Arc<dyn Queue + Send + Sync> = Arc::clone(q) as _;
        WorkerPool::new(q_dyn, handler, idem, "runs", 2).with_idle_sleep(Duration::from_millis(20))
    }

    #[tokio::test]
    async fn the_pool_consumes_through_receive_and_stops_receiving_on_stop() {
        let q = Arc::new(CountingQueue::default());
        q.publish("runs", Message::new(json!({}))).await.unwrap();
        let handled = Arc::new(AtomicUsize::new(0));
        let running = counting_pool(&q, &handled).start();
        tokio::time::sleep(Duration::from_millis(200)).await;
        running.stop().await;

        assert_eq!(handled.load(Ordering::Relaxed), 1);
        assert_eq!(q.depth("runs").await.unwrap(), 0);
        assert_eq!(q.polls.load(Ordering::Relaxed), 0, "plus de polling");
        // 2 tâches, 200 ms, 20 ms par tour : une file qui rend la main sans
        // attendre ne fait pas tourner la boucle à vide.
        let receives = q.receives.load(Ordering::Relaxed);
        assert!((2..=40).contains(&receives), "{receives} receive");
        assert_eq!(*q.stopped.lock().unwrap(), vec!["runs".to_string()]);
    }

    #[tokio::test]
    async fn a_bounded_stop_also_stops_receiving() {
        let q = Arc::new(CountingQueue::default());
        let handled = Arc::new(AtomicUsize::new(0));
        let mut running = counting_pool(&q, &handled).start();
        assert!(running.stop_within(Duration::from_secs(5)).await);
        assert_eq!(*q.stopped.lock().unwrap(), vec!["runs".to_string()]);
    }

    // ───── F5.2.c — idempotence ─────

    #[tokio::test]
    async fn duplicate_idempotency_key_skips_handler() {
        let q = Arc::new(InMemoryQueue::new());
        let msg = Message::new(json!({ "n": 1 })).with_header("idempotency-key", "abc");
        q.publish("runs", msg.clone()).await.unwrap();
        q.publish("runs", msg).await.unwrap();

        // Compteur d'appels au handler
        let call_count = Arc::new(Mutex::new(0_u32));
        let cc = Arc::clone(&call_count);
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(move |_d: &Delivery| {
            *cc.lock().unwrap() += 1;
            HandlerVerdict::Ok
        });
        let idem: Arc<dyn IdempotencyStore + Send + Sync> =
            Arc::new(InMemoryIdempotencyStore::new());
        let pool = WorkerPool::new(
            Arc::clone(&q) as Arc<dyn Queue + Send + Sync>,
            handler,
            idem,
            "runs",
            1,
        );

        let first = pool.process_one().await.unwrap();
        let second = pool.process_one().await.unwrap();
        assert_eq!(first, ProcessOutcome::Handled(HandlerVerdict::Ok));
        assert_eq!(second, ProcessOutcome::Deduplicated);
        // Handler appelé une seule fois
        assert_eq!(*call_count.lock().unwrap(), 1);
        // Les deux messages ont été ack'és → file vide, pas de DLQ
        assert_eq!(q.depth("runs").await.unwrap(), 0);
        assert_eq!(q.depth("runs.dlq").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn different_idempotency_keys_both_processed() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish(
            "runs",
            Message::new(json!({})).with_header("idempotency-key", "a"),
        )
        .await
        .unwrap();
        q.publish(
            "runs",
            Message::new(json!({})).with_header("idempotency-key", "b"),
        )
        .await
        .unwrap();

        let call_count = Arc::new(Mutex::new(0_u32));
        let cc = Arc::clone(&call_count);
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(move |_d: &Delivery| {
            *cc.lock().unwrap() += 1;
            HandlerVerdict::Ok
        });
        let pool = WorkerPool::new(
            Arc::clone(&q) as Arc<dyn Queue + Send + Sync>,
            handler,
            Arc::new(InMemoryIdempotencyStore::new()),
            "runs",
            1,
        );

        pool.process_one().await.unwrap();
        pool.process_one().await.unwrap();
        assert_eq!(*call_count.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn missing_idempotency_key_does_not_dedupe() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({}))).await.unwrap();
        q.publish("runs", Message::new(json!({}))).await.unwrap();
        let call_count = Arc::new(Mutex::new(0_u32));
        let cc = Arc::clone(&call_count);
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(move |_d: &Delivery| {
            *cc.lock().unwrap() += 1;
            HandlerVerdict::Ok
        });
        let pool = WorkerPool::new(
            Arc::clone(&q) as Arc<dyn Queue + Send + Sync>,
            handler,
            Arc::new(InMemoryIdempotencyStore::new()),
            "runs",
            1,
        );
        pool.process_one().await.unwrap();
        pool.process_one().await.unwrap();
        assert_eq!(*call_count.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn in_memory_idempotency_marks_only_on_demand() {
        let s = InMemoryIdempotencyStore::new();
        assert!(!s.is_done("k"));
        assert!(!s.is_done("k"), "consulter ne marque pas");
        s.mark_done("k");
        assert!(s.is_done("k"));
        // Autre clé indépendante
        assert!(!s.is_done("other"));
    }

    #[tokio::test]
    async fn retried_message_is_handled_again_on_redelivery() {
        // Régression #174 : la clé était marquée avant le handler, si bien
        // qu'un `Retry` redélivré passait pour un doublon et était ack'é.
        let q = Arc::new(InMemoryQueue::new());
        q.publish(
            "runs",
            Message::new(json!({})).with_header("idempotency-key", "run-1"),
        )
        .await
        .unwrap();
        let calls = Arc::new(Mutex::new(0_u32));
        let c = Arc::clone(&calls);
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(move |_d: &Delivery| {
            let mut n = c.lock().unwrap();
            *n += 1;
            if *n == 1 {
                HandlerVerdict::Retry
            } else {
                HandlerVerdict::Ok
            }
        });
        let pool = WorkerPool::new(
            Arc::clone(&q) as Arc<dyn Queue + Send + Sync>,
            handler,
            Arc::new(InMemoryIdempotencyStore::new()),
            "runs",
            1,
        );

        assert_eq!(
            pool.process_one().await.unwrap(),
            ProcessOutcome::Handled(HandlerVerdict::Retry)
        );
        assert_eq!(
            pool.process_one().await.unwrap(),
            ProcessOutcome::Handled(HandlerVerdict::Ok),
            "le Retry doit être retraité"
        );
        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(q.depth("runs").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn dead_lettered_key_is_not_handled_again() {
        let q = Arc::new(InMemoryQueue::new());
        let msg = Message::new(json!({})).with_header("idempotency-key", "run-x");
        q.publish("runs", msg.clone()).await.unwrap();
        q.publish("runs", msg).await.unwrap();
        let (pool, idem) = build(Arc::clone(&q), HandlerVerdict::DeadLetter);
        assert_eq!(
            pool.process_one().await.unwrap(),
            ProcessOutcome::Handled(HandlerVerdict::DeadLetter)
        );
        assert!(idem.is_done("run-x"));
        assert_eq!(
            pool.process_one().await.unwrap(),
            ProcessOutcome::Deduplicated
        );
    }

    // ───── F5.2.b — pool concurrent + shutdown ─────

    #[tokio::test]
    async fn pool_drains_all_messages_with_multiple_threads() {
        let q = Arc::new(InMemoryQueue::new());
        const N: usize = 200;
        for i in 0..N {
            q.publish("runs", Message::new(json!({ "n": i })))
                .await
                .unwrap();
        }
        // Handler qui compte les appels
        let processed = Arc::new(Mutex::new(0_u32));
        let pc = Arc::clone(&processed);
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(move |_d: &Delivery| {
            *pc.lock().unwrap() += 1;
            HandlerVerdict::Ok
        });
        let pool = WorkerPool::new(
            Arc::clone(&q) as Arc<dyn Queue + Send + Sync>,
            handler,
            Arc::new(InMemoryIdempotencyStore::new()),
            "runs",
            4,
        )
        .with_idle_sleep(Duration::from_millis(1));
        let running = pool.start();
        assert_eq!(running.task_count(), 4);

        // Attendre que la file soit drainée (avec garde-fou anti-flake)
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while q.depth("runs").await.unwrap() != 0 {
            if std::time::Instant::now() > deadline {
                panic!("file non drainée dans le délai");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Petit délai pour laisser les threads finir le dernier ack
        tokio::time::sleep(Duration::from_millis(50)).await;
        running.stop().await;
        assert_eq!(*processed.lock().unwrap() as usize, N);
        // Pas de message orphelin en DLQ
        assert_eq!(q.depth("runs.dlq").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn pool_stops_cleanly_when_signaled() {
        let q = Arc::new(InMemoryQueue::new());
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(|_d: &Delivery| HandlerVerdict::Ok);
        let pool = WorkerPool::new(
            Arc::clone(&q) as Arc<dyn Queue + Send + Sync>,
            handler,
            Arc::new(InMemoryIdempotencyStore::new()),
            "runs",
            2,
        )
        .with_idle_sleep(Duration::from_millis(5));
        let running = pool.start();
        // Pas de message — les workers restent en idle
        tokio::time::sleep(Duration::from_millis(20)).await;
        running.stop().await;
        // Si on arrive ici sans hang, stop() a join()é proprement.
    }

    #[tokio::test]
    async fn concurrency_zero_clamps_to_one() {
        let q = Arc::new(InMemoryQueue::new());
        let handler: Arc<dyn Handler + Send + Sync> = Arc::new(|_d: &Delivery| HandlerVerdict::Ok);
        let pool = WorkerPool::new(
            Arc::clone(&q) as Arc<dyn Queue + Send + Sync>,
            handler,
            Arc::new(InMemoryIdempotencyStore::new()),
            "runs",
            0,
        )
        .with_idle_sleep(Duration::from_millis(5));
        let running = pool.start();
        assert_eq!(running.task_count(), 1);
        running.stop().await;
    }

    // ───── garde-fou : Handler depuis closure compile bien ─────

    #[tokio::test]
    async fn closure_is_a_handler() {
        let h = |_d: &Delivery| HandlerVerdict::Ok;
        let delivery = Delivery {
            id: DeliveryId::new(1),
            queue: "x".into(),
            message: Message::new(json!({})),
            redeliveries: 0,
        };
        assert_eq!(h.handle(&delivery).await, HandlerVerdict::Ok);
    }
}

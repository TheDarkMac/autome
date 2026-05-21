//! Worker pool (F5.2).
//!
//! Consomme une [`autome_core::Queue`] (F5.1), passe chaque livraison à un
//! [`Handler`], et applique le verdict (`ack` / `nack(Requeue)` /
//! `nack(DeadLetter)`).
//!
//! L'implémentation est synchrone (`std::thread` + `Arc<AtomicBool>` pour le
//! shutdown) — pas de dépendance à `tokio` côté worker. Quand on branchera
//! le client `lapin` réel (qui est async), une fine couche d'adaptation
//! (un thread bloqueur qui tourne un runtime mono-thread) suffira ; le
//! contrat de cette PR reste indépendant du transport choisi.
//!
//! ## Architecture
//!
//! - [`Handler`] : trait que l'appelant implémente pour traiter un message.
//!   Une impl blanket existe pour toute closure `Fn(&Delivery) -> HandlerVerdict`.
//! - [`HandlerVerdict`] : résultat (`Ok` / `Retry` / `DeadLetter`) que le
//!   pool traduit en `ack` ou `nack`.
//! - [`IdempotencyStore`] (F5.2.c) : déduplication par `idempotency-key`
//!   présent dans les en-têtes du [`Message`]. L'impl `InMemoryIdempotencyStore`
//!   garde un `HashSet` sous `Mutex` ; un store persistant (Redis, base)
//!   viendra avec F5.4.
//! - [`WorkerPool::process_one`] : unité atomique (poll + handle + ack/nack),
//!   testable sans threads.
//! - [`WorkerPool::start`] : spawne `concurrency` threads qui appellent
//!   `process_one` en boucle. Renvoie un [`RunningPool`] dont `stop()`
//!   signale l'arrêt et `join()` proprement les threads.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use autome_core::{Delivery, Nack, Queue, Result};

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
pub trait Handler {
    /// Traite `delivery` et retourne le verdict (ack/retry/dlq).
    fn handle(&self, delivery: &Delivery) -> HandlerVerdict;
}

/// Tout `Fn(&Delivery) -> HandlerVerdict` est automatiquement un `Handler`.
impl<F> Handler for F
where
    F: Fn(&Delivery) -> HandlerVerdict,
{
    fn handle(&self, delivery: &Delivery) -> HandlerVerdict {
        (self)(delivery)
    }
}

/// Déduplication des messages déjà traités (F5.2.c).
///
/// Le worker pool consulte le store pour la clé `idempotency-key` (si
/// présente dans les en-têtes du [`autome_core::Message`]). Si la clé est
/// déjà vue, le message est `ack`'é sans appeler le handler.
pub trait IdempotencyStore {
    /// `true` si `key` a déjà été marquée (le handler doit être *skip*) ;
    /// `false` sinon, et la clé est marquée comme vue.
    fn check_or_mark(&self, key: &str) -> bool;
}

/// Store de déduplication in-process (`HashSet` sous `Mutex`).
///
/// Sert tests + mode dev. Un store persistant (base, Redis) viendra avec
/// F5.4 (persistance d'état d'exécution).
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
    fn check_or_mark(&self, key: &str) -> bool {
        let mut seen = self.seen.lock().expect("idempotency lock empoisonné");
        !seen.insert(key.to_string())
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
        Self {
            queue,
            handler,
            idempotency,
            queue_name: queue_name.into(),
            concurrency: concurrency.max(1),
            poll_idle_sleep: Duration::from_millis(50),
        }
    }

    /// Builder : durée de pause entre deux polls quand la file est vide.
    pub fn with_idle_sleep(mut self, dur: Duration) -> Self {
        self.poll_idle_sleep = dur;
        self
    }

    /// Traite UN message si disponible. Méthode synchrone, exposée pour les
    /// tests et pour piloter le worker en mode déterministe.
    pub fn process_one(&self) -> Result<ProcessOutcome> {
        let Some(delivery) = self.queue.poll(&self.queue_name)? else {
            return Ok(ProcessOutcome::Idle);
        };
        // Idempotence (F5.2.c) : si le message porte une clé déjà vue,
        // ack sans appeler le handler.
        if let Some(key) = delivery.message.headers.get("idempotency-key").cloned() {
            if self.idempotency.check_or_mark(&key) {
                self.queue.ack(&delivery.id)?;
                return Ok(ProcessOutcome::Deduplicated);
            }
        }
        let verdict = self.handler.handle(&delivery);
        match &verdict {
            HandlerVerdict::Ok => self.queue.ack(&delivery.id)?,
            HandlerVerdict::Retry => self.queue.nack(&delivery.id, Nack::Requeue)?,
            HandlerVerdict::DeadLetter => self.queue.nack(&delivery.id, Nack::DeadLetter)?,
        }
        Ok(ProcessOutcome::Handled(verdict))
    }

    /// Démarre `self.concurrency` threads workers. Renvoie un [`RunningPool`]
    /// pour stopper proprement (signal d'arrêt + join).
    pub fn start(self) -> RunningPool {
        let shutdown = Arc::new(AtomicBool::new(false));
        let pool = Arc::new(self);
        let mut handles = Vec::with_capacity(pool.concurrency);
        for i in 0..pool.concurrency {
            let pool = Arc::clone(&pool);
            let stop = Arc::clone(&shutdown);
            let handle = thread::Builder::new()
                .name(format!("autome-worker-{i}"))
                .spawn(move || worker_loop(&pool, &stop, i))
                .expect("spawn worker thread");
            handles.push(handle);
        }
        RunningPool { handles, shutdown }
    }
}

fn worker_loop(pool: &WorkerPool, shutdown: &AtomicBool, id: usize) {
    while !shutdown.load(Ordering::Acquire) {
        match pool.process_one() {
            Ok(ProcessOutcome::Idle) => thread::sleep(pool.poll_idle_sleep),
            Ok(_) => {}
            Err(e) => {
                eprintln!("[autome-worker-{id}] queue error: {e}");
                thread::sleep(pool.poll_idle_sleep);
            }
        }
    }
}

/// Handle d'un pool en cours d'exécution. Doit être stoppé proprement par
/// [`RunningPool::stop`] (sinon `Drop` log un warning, mais ne join pas —
/// les threads continueraient en arrière-plan jusqu'à arrêt du process).
#[must_use = "RunningPool doit être stoppé via `.stop()` pour arrêter proprement les threads"]
pub struct RunningPool {
    handles: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl RunningPool {
    /// Signale l'arrêt et `join()` tous les threads. Méthode consommante :
    /// après cet appel le pool n'est plus utilisable.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Vider `handles` AVANT que `Drop` voie un Vec non-vide (sinon
        // double-signalement + warning à tort).
        let handles = std::mem::take(&mut self.handles);
        for handle in handles {
            let _ = handle.join();
        }
    }

    /// Nombre de threads vivants (utile pour les assertions de tests).
    pub fn thread_count(&self) -> usize {
        self.handles.len()
    }
}

impl Drop for RunningPool {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            // L'appelant a oublié `stop()` : on signale l'arrêt — les
            // `JoinHandle` sont dropés (= threads détachées), elles sortent
            // à leur prochaine itération de boucle.
            self.shutdown.store(true, Ordering::Release);
            eprintln!(
                "RunningPool drop sans stop() — {} thread(s) signalés mais détachés",
                self.handles.len()
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

    // ───── F5.2.a — process_one ─────

    #[test]
    fn process_one_empty_queue_is_idle() {
        let q = Arc::new(InMemoryQueue::new());
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::Ok);
        assert_eq!(pool.process_one().unwrap(), ProcessOutcome::Idle);
    }

    #[test]
    fn process_one_ok_acks_and_drains_queue() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({ "n": 1 }))).unwrap();
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::Ok);
        let outcome = pool.process_one().unwrap();
        assert_eq!(outcome, ProcessOutcome::Handled(HandlerVerdict::Ok));
        // Queue principale vide, DLQ aussi (rien d'in-flight non plus puisque ack)
        assert_eq!(q.depth("runs").unwrap(), 0);
        assert_eq!(q.depth("runs.dlq").unwrap(), 0);
    }

    #[test]
    fn process_one_retry_requeues_and_increments_redeliveries() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({}))).unwrap();
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::Retry);
        let outcome = pool.process_one().unwrap();
        assert_eq!(outcome, ProcessOutcome::Handled(HandlerVerdict::Retry));
        // Le message est de retour
        assert_eq!(q.depth("runs").unwrap(), 1);
        let d = q.poll("runs").unwrap().unwrap();
        assert_eq!(d.redeliveries, 1);
    }

    #[test]
    fn process_one_dead_letter_moves_to_dlq() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({ "n": 9 }))).unwrap();
        let (pool, _) = build(Arc::clone(&q), HandlerVerdict::DeadLetter);
        let outcome = pool.process_one().unwrap();
        assert_eq!(outcome, ProcessOutcome::Handled(HandlerVerdict::DeadLetter));
        assert_eq!(q.depth("runs").unwrap(), 0);
        assert_eq!(q.depth("runs.dlq").unwrap(), 1);
    }

    // ───── F5.2.c — idempotence ─────

    #[test]
    fn duplicate_idempotency_key_skips_handler() {
        let q = Arc::new(InMemoryQueue::new());
        let msg = Message::new(json!({ "n": 1 })).with_header("idempotency-key", "abc");
        q.publish("runs", msg.clone()).unwrap();
        q.publish("runs", msg).unwrap();

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

        let first = pool.process_one().unwrap();
        let second = pool.process_one().unwrap();
        assert_eq!(first, ProcessOutcome::Handled(HandlerVerdict::Ok));
        assert_eq!(second, ProcessOutcome::Deduplicated);
        // Handler appelé une seule fois
        assert_eq!(*call_count.lock().unwrap(), 1);
        // Les deux messages ont été ack'és → file vide, pas de DLQ
        assert_eq!(q.depth("runs").unwrap(), 0);
        assert_eq!(q.depth("runs.dlq").unwrap(), 0);
    }

    #[test]
    fn different_idempotency_keys_both_processed() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish(
            "runs",
            Message::new(json!({})).with_header("idempotency-key", "a"),
        )
        .unwrap();
        q.publish(
            "runs",
            Message::new(json!({})).with_header("idempotency-key", "b"),
        )
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

        pool.process_one().unwrap();
        pool.process_one().unwrap();
        assert_eq!(*call_count.lock().unwrap(), 2);
    }

    #[test]
    fn missing_idempotency_key_does_not_dedupe() {
        let q = Arc::new(InMemoryQueue::new());
        q.publish("runs", Message::new(json!({}))).unwrap();
        q.publish("runs", Message::new(json!({}))).unwrap();
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
        pool.process_one().unwrap();
        pool.process_one().unwrap();
        assert_eq!(*call_count.lock().unwrap(), 2);
    }

    #[test]
    fn in_memory_idempotency_returns_false_first_then_true() {
        let s = InMemoryIdempotencyStore::new();
        assert!(!s.check_or_mark("k"));
        assert!(s.check_or_mark("k"));
        assert!(s.check_or_mark("k"));
        // Autre clé indépendante
        assert!(!s.check_or_mark("other"));
    }

    // ───── F5.2.b — pool concurrent + shutdown ─────

    #[test]
    fn pool_drains_all_messages_with_multiple_threads() {
        let q = Arc::new(InMemoryQueue::new());
        const N: usize = 200;
        for i in 0..N {
            q.publish("runs", Message::new(json!({ "n": i }))).unwrap();
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
        assert_eq!(running.thread_count(), 4);

        // Attendre que la file soit drainée (avec garde-fou anti-flake)
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while q.depth("runs").unwrap() != 0 {
            if std::time::Instant::now() > deadline {
                panic!("file non drainée dans le délai");
            }
            thread::sleep(Duration::from_millis(5));
        }
        // Petit délai pour laisser les threads finir le dernier ack
        thread::sleep(Duration::from_millis(50));
        running.stop();
        assert_eq!(*processed.lock().unwrap() as usize, N);
        // Pas de message orphelin en DLQ
        assert_eq!(q.depth("runs.dlq").unwrap(), 0);
    }

    #[test]
    fn pool_stops_cleanly_when_signaled() {
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
        thread::sleep(Duration::from_millis(20));
        running.stop();
        // Si on arrive ici sans hang, stop() a join()é proprement.
    }

    #[test]
    fn concurrency_zero_clamps_to_one() {
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
        assert_eq!(running.thread_count(), 1);
        running.stop();
    }

    // ───── garde-fou : Handler depuis closure compile bien ─────

    #[test]
    fn closure_is_a_handler() {
        let h = |_d: &Delivery| HandlerVerdict::Ok;
        let delivery = Delivery {
            id: DeliveryId::new(1),
            queue: "x".into(),
            message: Message::new(json!({})),
            redeliveries: 0,
        };
        assert_eq!(h.handle(&delivery), HandlerVerdict::Ok);
    }
}

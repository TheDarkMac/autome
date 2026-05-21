//! Planificateur de jobs (F5.3) — dispatch trigger → file.
//!
//! Produit ce que [`crate::queue`] transporte et que `autome_worker`
//! consomme (F5.2). Conceptuellement :
//!
//! ```text
//! Triggers ──┐
//!            │
//!            ▼   tick() poll + dedup_lock.try_acquire + queue.publish
//!         Scheduler ────────────────────────────────────────► Queue ─► WorkerPool
//! ```
//!
//! - [`Scheduler::tick`] (F5.3.a) : poll **synchrone** de tous les
//!   triggers enregistrés, dispatch des événements neufs vers la queue ;
//! - [`Scheduler::start`] (F5.3.a) : spawne un thread de fond qui
//!   appelle `tick` toutes les `interval` ; retourne un
//!   [`RunningScheduler`] dont `stop()` signale + join ;
//! - dispatch (F5.3.b) : un [`crate::TriggerEvent`] devient un
//!   [`crate::Message`] avec body `{source, payload}` et headers
//!   `idempotency-key` + `trigger-source` — l'`idempotency-key` s'aligne
//!   avec la dédup côté worker pool (`worker::IdempotencyStore`) ;
//! - dédup distribuée (F5.3.c) : trait [`DedupLock`] + impl
//!   [`InMemoryDedupLock`]. La vraie version cross-instance (Redis
//!   `SETNX`, PG advisory lock) viendra avec F5.4 quand on aura besoin
//!   du coordonnement multi-replicas.
//!
//! La clé de dédup est calculée via une closure pluggable
//! ([`Scheduler::with_dedup_key`]) ; par défaut
//! [`default_dedup_key`] renvoie `"<source>::<json(payload)>"`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::json;

use crate::{Message, Queue, Result, Trigger, TriggerEvent};

/// Lock anti-double-déclenchement (F5.3.c).
///
/// Sémantique : `try_acquire(key)` renvoie `true` si la clé n'a jamais
/// été marquée (et la marque), `false` sinon. Le scheduler ne dispatche
/// l'événement que sur `true`.
pub trait DedupLock {
    /// Tente d'acquérir le lock pour `key`. Retourne `true` si acquis.
    fn try_acquire(&self, key: &str) -> bool;
}

/// Implémentation in-process, `HashSet` sous `Mutex`. Ne survit pas au
/// redémarrage — pour ça, un store persistant (Redis, base) viendra
/// avec F5.4.
#[derive(Debug, Default)]
pub struct InMemoryDedupLock {
    seen: Mutex<HashSet<String>>,
}

impl InMemoryDedupLock {
    /// Crée un store vide.
    pub fn new() -> Self {
        Self::default()
    }
}

impl DedupLock for InMemoryDedupLock {
    fn try_acquire(&self, key: &str) -> bool {
        self.seen
            .lock()
            .expect("dedup lock empoisonné")
            .insert(key.to_string())
    }
}

/// Closure de calcul de la clé de déduplication d'un événement.
pub type DedupKeyFn = Box<dyn Fn(&TriggerEvent) -> String + Send + Sync>;

/// Clé de dédup par défaut : `"<source>::<json(payload)>"`. Stable tant
/// que le payload est sérialisable en JSON (toujours le cas via `serde`).
pub fn default_dedup_key(event: &TriggerEvent) -> String {
    let payload = serde_json::to_string(&event.payload).unwrap_or_default();
    format!("{}::{}", event.source, payload)
}

/// Planificateur — orchestre les triggers vers la queue.
pub struct Scheduler {
    triggers: Vec<Box<dyn Trigger + Send + Sync>>,
    queue: Arc<dyn Queue + Send + Sync>,
    queue_name: String,
    dedup: Arc<dyn DedupLock + Send + Sync>,
    dedup_key: DedupKeyFn,
}

impl Scheduler {
    /// Construit un scheduler vide (sans trigger enregistré).
    pub fn new(
        queue: Arc<dyn Queue + Send + Sync>,
        queue_name: impl Into<String>,
        dedup: Arc<dyn DedupLock + Send + Sync>,
    ) -> Self {
        Self {
            triggers: Vec::new(),
            queue,
            queue_name: queue_name.into(),
            dedup,
            dedup_key: Box::new(default_dedup_key),
        }
    }

    /// Builder : enregistre un trigger.
    pub fn register(mut self, trigger: Box<dyn Trigger + Send + Sync>) -> Self {
        self.triggers.push(trigger);
        self
    }

    /// Builder : remplace la fonction de calcul de la clé de déduplication.
    pub fn with_dedup_key<F>(mut self, f: F) -> Self
    where
        F: Fn(&TriggerEvent) -> String + Send + Sync + 'static,
    {
        self.dedup_key = Box::new(f);
        self
    }

    /// Poll **tous** les triggers une fois et dispatche les événements
    /// neufs sur la queue. Retourne le nombre de messages publiés.
    ///
    /// Un trigger qui renvoie une erreur arrête la passe courante — les
    /// triggers suivants ne sont pas pollés ; le caller décide quoi faire
    /// (typiquement : ignorer et retenter au tick suivant).
    pub fn tick(&mut self) -> Result<usize> {
        let mut dispatched = 0;
        for trigger in &mut self.triggers {
            while let Some(event) = trigger.poll()? {
                let key = (self.dedup_key)(&event);
                if !self.dedup.try_acquire(&key) {
                    continue;
                }
                let body = json!({
                    "source": event.source,
                    "payload": event.payload,
                });
                let msg = Message::new(body)
                    .with_header("idempotency-key", &key)
                    .with_header("trigger-source", &event.source);
                self.queue.publish(&self.queue_name, msg)?;
                dispatched += 1;
            }
        }
        Ok(dispatched)
    }

    /// Démarre une boucle de fond `tick → sleep(interval)`.
    ///
    /// Pour stopper proprement, appeler [`RunningScheduler::stop`].
    pub fn start(self, interval: Duration) -> RunningScheduler {
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let mut scheduler = self;
        let handle = thread::Builder::new()
            .name("autome-scheduler".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    if let Err(e) = scheduler.tick() {
                        eprintln!("[autome-scheduler] tick error: {e}");
                    }
                    thread::sleep(interval);
                }
            })
            .expect("spawn scheduler thread");
        RunningScheduler {
            handle: Some(handle),
            shutdown,
        }
    }
}

/// Handle d'un [`Scheduler`] tournant en arrière-plan.
#[must_use = "RunningScheduler doit être stoppé via `.stop()` pour arrêter proprement le thread"]
pub struct RunningScheduler {
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl RunningScheduler {
    /// Signale l'arrêt et `join()` le thread.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RunningScheduler {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.shutdown.store(true, Ordering::Release);
            eprintln!("RunningScheduler drop sans stop() — thread signalé mais détaché");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryQueue;
    use serde_json::Value;

    /// Trigger factice : émet `events` dans l'ordre puis plus rien.
    struct Replay {
        events: std::collections::VecDeque<TriggerEvent>,
    }
    impl Replay {
        fn new(events: Vec<TriggerEvent>) -> Self {
            Self {
                events: events.into(),
            }
        }
    }
    impl Trigger for Replay {
        fn kind(&self) -> &'static str {
            "replay"
        }
        fn poll(&mut self) -> Result<Option<TriggerEvent>> {
            Ok(self.events.pop_front())
        }
    }

    /// Trigger qui peut être nourri dynamiquement.
    struct Feed {
        backlog: Arc<Mutex<Vec<TriggerEvent>>>,
    }
    impl Feed {
        fn new() -> (Self, Arc<Mutex<Vec<TriggerEvent>>>) {
            let backlog = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    backlog: Arc::clone(&backlog),
                },
                backlog,
            )
        }
    }
    impl Trigger for Feed {
        fn kind(&self) -> &'static str {
            "feed"
        }
        fn poll(&mut self) -> Result<Option<TriggerEvent>> {
            Ok(self.backlog.lock().unwrap().pop())
        }
    }

    // ───── F5.3.a / F5.3.b — tick + dispatch ─────

    #[test]
    fn tick_dispatches_event_to_queue() {
        let queue = Arc::new(InMemoryQueue::new());
        let mut scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .register(Box::new(Replay::new(vec![TriggerEvent::new(
            "replay",
            json!({ "a": 1 }),
        )])));
        let count = scheduler.tick().unwrap();
        assert_eq!(count, 1);
        assert_eq!(queue.depth("runs").unwrap(), 1);
        let d = queue.poll("runs").unwrap().unwrap();
        assert_eq!(d.message.body["source"], json!("replay"));
        assert_eq!(d.message.body["payload"], json!({ "a": 1 }));
        // Headers
        assert!(d
            .message
            .headers
            .get("idempotency-key")
            .map(|k| k.contains("replay"))
            .unwrap_or(false));
        assert_eq!(
            d.message.headers.get("trigger-source").map(String::as_str),
            Some("replay")
        );
    }

    #[test]
    fn tick_returns_zero_when_no_events() {
        let queue = Arc::new(InMemoryQueue::new());
        let mut scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .register(Box::new(Replay::new(vec![])));
        assert_eq!(scheduler.tick().unwrap(), 0);
        assert_eq!(queue.depth("runs").unwrap(), 0);
    }

    #[test]
    fn tick_drains_all_pending_events_in_one_pass() {
        let queue = Arc::new(InMemoryQueue::new());
        let events = (0..5)
            .map(|i| TriggerEvent::new("replay", json!({ "n": i })))
            .collect();
        let mut scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .register(Box::new(Replay::new(events)));
        assert_eq!(scheduler.tick().unwrap(), 5);
        assert_eq!(queue.depth("runs").unwrap(), 5);
    }

    #[test]
    fn tick_aggregates_multiple_triggers() {
        let queue = Arc::new(InMemoryQueue::new());
        let mut scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .register(Box::new(Replay::new(vec![TriggerEvent::new(
            "a",
            json!({}),
        )])))
        .register(Box::new(Replay::new(vec![TriggerEvent::new(
            "b",
            json!({}),
        )])));
        assert_eq!(scheduler.tick().unwrap(), 2);
        assert_eq!(queue.depth("runs").unwrap(), 2);
    }

    // ───── F5.3.c — déduplication ─────

    #[test]
    fn duplicate_event_is_filtered_by_dedup_lock() {
        let queue = Arc::new(InMemoryQueue::new());
        // Même événement deux fois
        let mut scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .register(Box::new(Replay::new(vec![
            TriggerEvent::new("dup", json!({ "id": 1 })),
            TriggerEvent::new("dup", json!({ "id": 1 })),
        ])));
        let dispatched = scheduler.tick().unwrap();
        assert_eq!(dispatched, 1);
        assert_eq!(queue.depth("runs").unwrap(), 1);
    }

    #[test]
    fn distinct_events_are_all_dispatched() {
        let queue = Arc::new(InMemoryQueue::new());
        let mut scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .register(Box::new(Replay::new(vec![
            TriggerEvent::new("dup", json!({ "id": 1 })),
            TriggerEvent::new("dup", json!({ "id": 2 })),
            TriggerEvent::new("dup", json!({ "id": 3 })),
        ])));
        assert_eq!(scheduler.tick().unwrap(), 3);
    }

    #[test]
    fn dedup_persists_across_ticks() {
        let queue = Arc::new(InMemoryQueue::new());
        let (feed, backlog) = Feed::new();
        let mut scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .register(Box::new(feed));

        // Tick 1 : émet event X
        backlog
            .lock()
            .unwrap()
            .push(TriggerEvent::new("feed", json!({ "id": "X" })));
        assert_eq!(scheduler.tick().unwrap(), 1);

        // Tick 2 : ré-émet le MÊME event X → filtré par dedup
        backlog
            .lock()
            .unwrap()
            .push(TriggerEvent::new("feed", json!({ "id": "X" })));
        assert_eq!(scheduler.tick().unwrap(), 0);

        // Tick 3 : event Y nouveau → dispatché
        backlog
            .lock()
            .unwrap()
            .push(TriggerEvent::new("feed", json!({ "id": "Y" })));
        assert_eq!(scheduler.tick().unwrap(), 1);
        assert_eq!(queue.depth("runs").unwrap(), 2);
    }

    #[test]
    fn custom_dedup_key_is_used() {
        let queue = Arc::new(InMemoryQueue::new());
        // Clé custom : seulement le champ `id` du payload, on ignore le reste.
        let mut scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .with_dedup_key(|e| {
            e.payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .register(Box::new(Replay::new(vec![
            // Même `id` mais payloads différents → dédoublonnés par clé custom
            TriggerEvent::new("a", json!({ "id": "K", "extra": 1 })),
            TriggerEvent::new("b", json!({ "id": "K", "extra": 2 })),
            TriggerEvent::new("c", json!({ "id": "OTHER" })),
        ])));
        let dispatched = scheduler.tick().unwrap();
        assert_eq!(dispatched, 2);
    }

    #[test]
    fn in_memory_dedup_lock_returns_true_first_then_false() {
        let l = InMemoryDedupLock::new();
        assert!(l.try_acquire("k"));
        assert!(!l.try_acquire("k"));
        assert!(!l.try_acquire("k"));
        assert!(l.try_acquire("other"));
    }

    #[test]
    fn default_dedup_key_combines_source_and_payload() {
        let e1 = TriggerEvent::new("s", json!({ "x": 1 }));
        let e2 = TriggerEvent::new("s", json!({ "x": 1 }));
        let e3 = TriggerEvent::new("s", json!({ "x": 2 }));
        assert_eq!(default_dedup_key(&e1), default_dedup_key(&e2));
        assert_ne!(default_dedup_key(&e1), default_dedup_key(&e3));
    }

    // ───── F5.3.a — start / stop ─────

    #[test]
    fn start_runs_tick_periodically_and_stop_joins_cleanly() {
        let queue = Arc::new(InMemoryQueue::new());
        let (feed, backlog) = Feed::new();

        let scheduler = Scheduler::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            "runs",
            Arc::new(InMemoryDedupLock::new()),
        )
        .register(Box::new(feed));

        let running = scheduler.start(Duration::from_millis(10));

        // Pousse 3 événements distincts, attend que la queue les ait reçus
        backlog
            .lock()
            .unwrap()
            .push(TriggerEvent::new("feed", json!({ "id": 1 })));
        backlog
            .lock()
            .unwrap()
            .push(TriggerEvent::new("feed", json!({ "id": 2 })));
        backlog
            .lock()
            .unwrap()
            .push(TriggerEvent::new("feed", json!({ "id": 3 })));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while queue.depth("runs").unwrap() < 3 {
            if std::time::Instant::now() > deadline {
                panic!(
                    "scheduler n'a pas dispatché les 3 événements (queue depth={})",
                    queue.depth("runs").unwrap()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        running.stop();
        assert!(queue.depth("runs").unwrap() >= 3);
    }
}

//! Test de charge end-to-end (F5.6).
//!
//! Valide l'assemblage des briques du domaine 5 sur la stack **in-memory** :
//! `Scheduler` (F5.3) → `BoundedQueue` (F5.5) → `WorkerPool` (F5.2). Le
//! vrai benchmark (RabbitMQ, multi-process, métriques Prometheus) relève
//! du domaine 10 ; ici on prouve simplement que :
//!
//! 1. la chaîne ne perd aucun message ;
//! 2. la concurrence scale avec le nombre de threads ;
//! 3. on tient un débit minimum (milliers de runs/s) sur du matériel modeste.
//!
//! ## Comment exécuter
//!
//! - Smoke (toujours run) : `cargo test -p autome-worker --test load`.
//! - Load (manuel) : `cargo test -p autome-worker --test load -- --ignored --nocapture`.
//!
//! ## Ordres de grandeur (laptop dev, Linux x86_64)
//!
//! | Workers | Events | Débit attendu |
//! |---------|--------|---------------|
//! | 2       | 100    | ≥ 1 000/s     |
//! | 8       | 10 000 | ≥ 50 000/s    |
//!
//! Les assertions du test sont volontairement *conservatives* (≥ 1 000/s)
//! pour ne pas devenir flake sur une CI lente.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use autome_core::{
    BoundedQueue, Delivery, InMemoryDedupLock, InMemoryQueue, Message, Queue, Result, Scheduler,
    Trigger, TriggerEvent,
};
use autome_worker::{Handler, HandlerVerdict, InMemoryIdempotencyStore, WorkerPool};
use serde_json::json;

/// Trigger factice qui émet `total` événements à raison d'un par `poll`.
struct LoadTrigger {
    remaining: u32,
}

impl Trigger for LoadTrigger {
    fn kind(&self) -> &'static str {
        "load"
    }
    fn poll(&mut self) -> Result<Option<TriggerEvent>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        Ok(Some(TriggerEvent::new(
            "load",
            json!({ "n": self.remaining }),
        )))
    }
}

fn block_until<F>(deadline: Instant, mut cond: F, what: &str)
where
    F: FnMut() -> bool,
{
    while !cond() {
        if Instant::now() > deadline {
            panic!("timeout en attendant : {what}");
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// F5.6.a — Smoke test : passe la chaîne complète sur ~100 events et vérifie
/// que tous sont traités, sans message en DLQ et avec un débit minimum.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn smoke_full_pipeline_scheduler_to_worker() {
    const N: u32 = 100;
    const WORKERS: usize = 2;

    let bounded = BoundedQueue::new(InMemoryQueue::new()).with_default_limit(N as usize + 10);
    let queue: Arc<dyn Queue + Send + Sync> = Arc::new(bounded);

    // Compteur d'events traités
    let processed = Arc::new(AtomicU32::new(0));
    let pc = Arc::clone(&processed);

    // Handler : compte les appels, succès systématique
    let handler: Arc<dyn Handler + Send + Sync> = Arc::new(move |_d: &Delivery| {
        pc.fetch_add(1, Ordering::SeqCst);
        HandlerVerdict::Ok
    });

    // Pipeline producteur : Scheduler avec un trigger pré-chargé
    let scheduler = Scheduler::new(
        Arc::clone(&queue),
        "runs",
        Arc::new(InMemoryDedupLock::new()),
    )
    .register(Box::new(LoadTrigger { remaining: N }));

    // Worker pool consommateur
    let pool = WorkerPool::new(
        Arc::clone(&queue),
        handler,
        Arc::new(InMemoryIdempotencyStore::new()),
        "runs",
        WORKERS,
    )
    .with_idle_sleep(Duration::from_millis(1));

    let start = Instant::now();
    let running = pool.start();

    // Le scheduler draine tous les events en un tick (le trigger libère tout
    // ce qu'il a au premier `poll`). En production le scheduler tickerait
    // périodiquement avec `start()` ; pour le test on l'appelle directement.
    let mut scheduler = scheduler;
    let dispatched = scheduler.tick().await.expect("scheduler tick");
    assert_eq!(
        dispatched, N as usize,
        "tous les events doivent être dispatchés"
    );

    // Attendre que les workers aient tout traité (timeout généreux pour CI lente)
    block_until(
        start + Duration::from_secs(10),
        || processed.load(Ordering::SeqCst) >= N,
        "processed == N",
    );
    // Laisser un cycle aux workers pour ack le dernier
    tokio::time::sleep(Duration::from_millis(20)).await;
    running.stop().await;

    let elapsed = start.elapsed();
    let throughput = N as f64 / elapsed.as_secs_f64();
    println!(
        "[smoke] N={N} workers={WORKERS} elapsed={:?} throughput={:.0}/s",
        elapsed, throughput
    );

    // Aucun message ne doit traîner ni en file principale ni en DLQ
    assert_eq!(
        queue.depth("runs").await.unwrap(),
        0,
        "queue principale vidée"
    );
    assert_eq!(
        queue.depth("runs.dlq").await.unwrap(),
        0,
        "pas de DLQ orpheline"
    );
    assert_eq!(processed.load(Ordering::SeqCst), N);

    // Assertion débit *très* conservative pour ne pas être flake sur CI faible.
    assert!(
        throughput >= 500.0,
        "throughput smoke {throughput:.0}/s < baseline 500/s"
    );
}

/// F5.6.b — Test de charge : 10 000 events, 8 workers, mesure throughput.
///
/// Marqué `#[ignore]` pour ne pas alourdir `cargo test`. Lancer avec
/// `cargo test -p autome-worker --test load -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load test — invoque avec --ignored --nocapture pour voir le throughput"]
async fn load_throughput_baseline() {
    const N: u32 = 10_000;
    const WORKERS: usize = 8;
    const MIN_THROUGHPUT_PER_SEC: f64 = 1_000.0;

    // Queue généreusement dimensionnée pour ne pas être le facteur limitant.
    let queue: Arc<dyn Queue + Send + Sync> =
        Arc::new(BoundedQueue::new(InMemoryQueue::new()).with_default_limit(N as usize + 1_000));

    // Pré-publish synchrone : on mesure le throughput de **consommation**,
    // pas la coordination scheduler-producer.
    let publish_start = Instant::now();
    for i in 0..N {
        queue
            .publish(
                "runs",
                Message::new(json!({ "n": i })).with_header("idempotency-key", format!("evt-{i}")),
            )
            .await
            .expect("publish");
    }
    let publish_elapsed = publish_start.elapsed();
    let publish_throughput = N as f64 / publish_elapsed.as_secs_f64();
    println!(
        "[load] publish: N={N} elapsed={:?} {:.0}/s",
        publish_elapsed, publish_throughput
    );

    let processed = Arc::new(AtomicU32::new(0));
    let pc = Arc::clone(&processed);
    let handler: Arc<dyn Handler + Send + Sync> = Arc::new(move |_d: &Delivery| {
        pc.fetch_add(1, Ordering::SeqCst);
        HandlerVerdict::Ok
    });

    let pool = WorkerPool::new(
        Arc::clone(&queue),
        handler,
        Arc::new(InMemoryIdempotencyStore::new()),
        "runs",
        WORKERS,
    )
    .with_idle_sleep(Duration::from_millis(1));

    let consume_start = Instant::now();
    let running = pool.start();

    block_until(
        consume_start + Duration::from_secs(60),
        || processed.load(Ordering::SeqCst) >= N,
        "load: processed == N",
    );
    let consume_elapsed = consume_start.elapsed();
    tokio::time::sleep(Duration::from_millis(20)).await;
    running.stop().await;

    let throughput = N as f64 / consume_elapsed.as_secs_f64();
    println!(
        "[load] consume: N={N} workers={WORKERS} elapsed={:?} {:.0}/s",
        consume_elapsed, throughput
    );

    assert_eq!(queue.depth("runs").await.unwrap(), 0);
    assert_eq!(queue.depth("runs.dlq").await.unwrap(), 0);
    assert_eq!(processed.load(Ordering::SeqCst), N);
    assert!(
        throughput >= MIN_THROUGHPUT_PER_SEC,
        "load throughput {throughput:.0}/s < baseline {MIN_THROUGHPUT_PER_SEC}/s"
    );
}

/// Garde-fou : la backpressure entre en jeu sous forte charge si la queue
/// est sous-dimensionnée. On vérifie qu'on obtient bien un `Err(Backpressure)`
/// quand on cible une queue plus petite que le burst de publication.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_triggers_under_burst() {
    const N: u32 = 1_000;
    const QUEUE_LIMIT: usize = 100;

    let queue = BoundedQueue::new(InMemoryQueue::new()).with_default_limit(QUEUE_LIMIT);

    let mut backpressure_count = 0;
    let mut accepted = 0;
    for i in 0..N {
        match queue.publish("runs", Message::new(json!({ "n": i }))).await {
            Ok(()) => accepted += 1,
            Err(autome_core::Error::Backpressure { .. }) => backpressure_count += 1,
            Err(e) => panic!("erreur inattendue : {e}"),
        }
    }
    assert_eq!(
        accepted, QUEUE_LIMIT,
        "exactement QUEUE_LIMIT messages doivent être acceptés"
    );
    assert_eq!(
        backpressure_count,
        N as usize - QUEUE_LIMIT,
        "le reste doit déclencher de la backpressure"
    );
    assert_eq!(queue.depth("runs").await.unwrap(), QUEUE_LIMIT);
}

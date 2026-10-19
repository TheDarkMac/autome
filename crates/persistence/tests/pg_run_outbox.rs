//! Outbox du dispatch et reprise des runs (#239) contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Applique les migrations.

use std::sync::Arc;

use autome_core::{
    outbox_idempotency_key, Message, OutboxMessage, RunCheckpoint, RunProgress, RunStateStore,
    RunStatus,
};
use autome_persistence::{connect, run_migrations, PgPool, PgRunStateStore};
use serde_json::json;

/// Chaque test repart d'un schéma vierge : on les sérialise (sinon un
/// `DROP SCHEMA` concurrent casserait le test voisin).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup() -> Option<(
    PgRunStateStore,
    PgPool,
    tokio::sync::MutexGuard<'static, ()>,
)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let guard = SERIAL.lock().await;
    let pool = connect(&url).expect("pool");
    {
        let client = pool.get().await.expect("conn");
        client
            .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .await
            .expect("reset");
    }
    run_migrations(&pool).await.expect("migrations");
    Some((PgRunStateStore::new(pool.clone()), pool, guard))
}

fn outbox_for(run_id: &str) -> OutboxMessage {
    OutboxMessage {
        run_id: run_id.into(),
        queue: "runs".into(),
        generation: 0,
        message: Message::new(json!({ "run_id": run_id, "payload": { "k": 1 } }))
            .with_header("idempotency-key", outbox_idempotency_key(run_id, 0))
            .with_header("trigger-source", "manual"),
    }
}

fn queued(run_id: &str, created_at_ms: i64) -> RunCheckpoint {
    RunCheckpoint::queued(run_id, "wf", 1, "manual", created_at_ms).with_owner(Some("alice".into()))
}

/// Run en file avec son message publié, puis pris par un worker à `100`.
async fn claimed(store: &PgRunStateStore, run_id: &str) -> RunCheckpoint {
    store
        .enqueue(&queued(run_id, 10), &outbox_for(run_id))
        .await
        .unwrap();
    store.mark_published(run_id, 0, 11).await.unwrap();
    store.try_claim(run_id, 100).await.unwrap().unwrap()
}

async fn outbox_rows(pool: &PgPool) -> i64 {
    let client = pool.get().await.unwrap();
    client
        .query_one("SELECT count(*) FROM run_outbox", &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn enqueue_is_atomic_and_pending_until_published() {
    let Some((store, pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    store
        .enqueue(&queued("b", 20), &outbox_for("b"))
        .await
        .unwrap();
    store
        .enqueue(&queued("a", 10), &outbox_for("a"))
        .await
        .unwrap();
    let run = store.load("a").await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Queued);
    assert_eq!(run.attempt, 0);
    assert_eq!(run.owner_id.as_deref(), Some("alice"));

    // Plus anciens d'abord, borne et seuil stricts.
    assert_eq!(
        store.pending_outbox(i64::MAX, 10).await.unwrap(),
        vec![outbox_for("a"), outbox_for("b")]
    );
    assert_eq!(store.pending_outbox(i64::MAX, 1).await.unwrap().len(), 1);
    assert!(store.pending_outbox(10, 10).await.unwrap().is_empty());

    // Id déjà pris : rien n'est écrit (ni run écrasé, ni second message).
    let mut clash = queued("a", 99);
    clash.workflow_id = "other".into();
    assert!(store.enqueue(&clash, &outbox_for("a")).await.is_err());
    assert_eq!(store.load("a").await.unwrap().unwrap().workflow_id, "wf");
    // Message sans run (clé étrangère) : la transaction entière échoue.
    let mut orphan = outbox_for("x");
    orphan.run_id = "ghost".into();
    assert!(store.enqueue(&queued("x", 5), &orphan).await.is_err());
    assert!(store.load("x").await.unwrap().is_none(), "run annulé avec");
    assert_eq!(outbox_rows(&pool).await, 2);

    store.mark_published("a", 0, 30).await.unwrap();
    // Mauvaise génération : sans effet.
    store.mark_published("b", 7, 30).await.unwrap();
    let pending: Vec<String> = store
        .pending_outbox(i64::MAX, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.run_id)
        .collect();
    assert_eq!(pending, vec!["b"]);
}

#[tokio::test]
async fn claims_progress_and_heartbeats_are_fenced_by_the_attempt() {
    let Some((store, _pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let first = claimed(&store, "r").await;
    assert_eq!(first.attempt, 1);
    assert_eq!(first.heartbeat_at_ms, Some(100));

    let starting = RunProgress::Starting { node: "a".into() };
    assert!(store.record_progress("r", 1, &starting, 150).await.unwrap());
    let cp = store.load("r").await.unwrap().unwrap();
    assert_eq!(cp.next_node.as_deref(), Some("a"));
    assert_eq!(cp.heartbeat_at_ms, Some(150));
    let cursor = json!({ "done": [{ "node": "a" }], "context": { "a": { "ok": true } } });
    let done = RunProgress::Done {
        resume_state: cursor.clone(),
    };
    assert!(store.record_progress("r", 1, &done, 160).await.unwrap());
    let cp = store.load("r").await.unwrap().unwrap();
    assert_eq!(cp.next_node, None);
    assert_eq!(cp.resume_state, Some(cursor.clone()));
    assert!(store.heartbeat("r", 1, 170).await.unwrap());

    // Autre tentative ou run inconnu : rien n'est écrit.
    assert!(!store.record_progress("r", 2, &starting, 180).await.unwrap());
    assert!(!store.heartbeat("r", 2, 180).await.unwrap());
    assert!(!store.heartbeat("ghost", 1, 180).await.unwrap());
    assert_eq!(
        store.load("r").await.unwrap().unwrap().heartbeat_at_ms,
        Some(170)
    );

    // Remise en file puis seconde prise : tentative 2, curseur conservé.
    store.requeue("r", 1, 200).await.unwrap().unwrap();
    assert!(!store.heartbeat("r", 1, 210).await.unwrap(), "plus running");
    let second = store.try_claim("r", 300).await.unwrap().unwrap();
    assert_eq!(second.attempt, 2);
    assert_eq!(second.started_at_ms, Some(300));
    assert_eq!(second.resume_state, Some(cursor));

    // L'état final de la première tentative est refusé, celui de la seconde
    // écrit, et le message disparaît avec le run terminé.
    let mut stale = first;
    stale.status = RunStatus::Succeeded;
    assert!(!store.finish(&stale).await.unwrap());
    assert_eq!(
        store.load("r").await.unwrap().unwrap().status,
        RunStatus::Running
    );
    let mut fin = second;
    fin.status = RunStatus::Succeeded;
    fin.finished_at_ms = Some(400);
    fin.resume_state = None;
    fin.outputs.insert("a".into(), json!({ "ok": true }));
    assert!(store.finish(&fin).await.unwrap());
    let got = store.load("r").await.unwrap().unwrap();
    assert_eq!(got.status, RunStatus::Succeeded);
    assert_eq!(got.resume_state, None);
    assert_eq!(got.outputs["a"], json!({ "ok": true }));
    assert!(store.requeue("r", 2, 500).await.unwrap().is_none());
    assert!(store.pending_outbox(i64::MAX, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn requeue_republishes_under_a_new_generation() {
    let Some((store, pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    claimed(&store, "r").await;
    store
        .record_progress("r", 1, &RunProgress::Starting { node: "b".into() }, 150)
        .await
        .unwrap();

    // Mauvaise tentative : refusée, le run reste `running`.
    assert!(store.requeue("r", 2, 200).await.unwrap().is_none());
    let msg = store.requeue("r", 1, 200).await.unwrap().unwrap();
    assert_eq!(msg.generation, 1);
    assert_eq!(msg.queue, "runs");
    assert_eq!(msg.message.body["payload"], json!({ "k": 1 }));
    assert_eq!(
        msg.message
            .headers
            .get("idempotency-key")
            .map(String::as_str),
        Some(outbox_idempotency_key("r", 1).as_str())
    );
    assert_eq!(
        msg.message
            .headers
            .get("trigger-source")
            .map(String::as_str),
        Some("manual")
    );
    let cp = store.load("r").await.unwrap().unwrap();
    assert_eq!(cp.status, RunStatus::Queued);
    assert_eq!(cp.heartbeat_at_ms, None);
    assert_eq!(cp.next_node.as_deref(), Some("b"), "nœud en cours gardé");

    // À publier de nouveau à partir de la remise en file ; un relais en
    // retard sur la génération 0 ne la masque pas.
    assert!(store.pending_outbox(200, 10).await.unwrap().is_empty());
    assert_eq!(store.pending_outbox(201, 10).await.unwrap(), vec![msg]);
    store.mark_published("r", 0, 210).await.unwrap();
    assert_eq!(store.pending_outbox(i64::MAX, 10).await.unwrap().len(), 1);
    store.mark_published("r", 1, 220).await.unwrap();
    assert!(store.pending_outbox(i64::MAX, 10).await.unwrap().is_empty());
    assert!(store.requeue("r", 1, 300).await.unwrap().is_none());

    // Run antérieur à l'outbox (pas de message) : pas de remise en file.
    store
        .checkpoint(&RunCheckpoint::queued("legacy", "wf", 1, "manual", 1))
        .await
        .unwrap();
    store.try_claim("legacy", 2).await.unwrap().unwrap();
    assert!(store.requeue("legacy", 1, 3).await.unwrap().is_none());
    assert_eq!(
        store.load("legacy").await.unwrap().unwrap().status,
        RunStatus::Running
    );
    assert_eq!(outbox_rows(&pool).await, 1);
}

#[tokio::test]
async fn concurrent_requeues_have_a_single_winner() {
    let Some((store, _pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    claimed(&store, "r").await;
    let store = Arc::new(store);
    let mut tasks = Vec::new();
    for i in 0..8 {
        let s = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            s.requeue("r", 1, 200 + i).await.unwrap()
        }));
    }
    let mut winners = 0;
    for t in tasks {
        winners += usize::from(t.await.unwrap().is_some());
    }
    assert_eq!(winners, 1);
    assert_eq!(
        store.pending_outbox(i64::MAX, 10).await.unwrap()[0].generation,
        1
    );
}

#[tokio::test]
async fn stale_runs_failures_and_erasure() {
    let Some((store, pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    claimed(&store, "old").await; // battement à 100
    claimed(&store, "fresh").await;
    assert!(store.heartbeat("fresh", 1, 500).await.unwrap());
    // Pris par un worker antérieur à #239 : pas de battement, ignoré.
    let mut legacy = RunCheckpoint::queued("legacy", "wf", 1, "manual", 1);
    legacy.status = RunStatus::Running;
    legacy.started_at_ms = Some(1);
    store.checkpoint(&legacy).await.unwrap();
    let stale: Vec<String> = store
        .list_stale(400)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.run_id)
        .collect();
    assert_eq!(stale, vec!["old"]);
    assert!(store.list_stale(100).await.unwrap().is_empty(), "strict");

    // Un run clos en échec perd son message.
    assert_eq!(outbox_rows(&pool).await, 2);
    assert!(store.fail_if_running("old", "x", 600).await.unwrap());
    assert!(!store.fail_if_running("old", "x", 700).await.unwrap());
    store.mark_failed("fresh", "x", 600).await.unwrap();
    assert_eq!(outbox_rows(&pool).await, 0);

    // Effacement du run (suppression de compte) : message en cascade.
    store
        .enqueue(&queued("e", 1), &outbox_for("e"))
        .await
        .unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute("DELETE FROM runs WHERE owner_id = 'alice'", &[])
        .await
        .unwrap();
    assert_eq!(outbox_rows(&pool).await, 0);
}

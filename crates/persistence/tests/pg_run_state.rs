//! Test d'intégration de [`PgRunStateStore`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Applique les migrations.

use autome_core::{Pagination, RunCheckpoint, RunStateStore, RunStatus};
use autome_persistence::{connect, run_migrations, PgRunStateStore};
use serde_json::json;

async fn setup() -> Option<PgRunStateStore> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    {
        let client = pool.get().await.expect("conn");
        client
            .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .await
            .expect("reset");
    }
    run_migrations(&pool).await.expect("migrations");
    Some(PgRunStateStore::new(pool))
}

#[tokio::test]
async fn checkpoint_load_orphans_failed_and_list() {
    let Some(store) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    // Inconnu.
    assert!(store.load("r1").await.unwrap().is_none());

    // Checkpoint Queued → load reconstruit fidèlement.
    let mut cp = RunCheckpoint::queued("r1", "wf", 1, "manual", 1_000);
    store.checkpoint(&cp).await.unwrap();
    let got = store.load("r1").await.unwrap().unwrap();
    assert_eq!(got, cp);

    // Transition Running + sorties (run_steps) — overwrite.
    cp.status = RunStatus::Running;
    cp.started_at_ms = Some(1_500);
    cp.outputs.insert("fetch".into(), json!({ "value": 21 }));
    cp.outputs.insert("double".into(), json!({ "result": 42 }));
    cp.next_node = Some("double".into());
    store.checkpoint(&cp).await.unwrap();
    let got = store.load("r1").await.unwrap().unwrap();
    assert_eq!(got.outputs.len(), 2);
    assert_eq!(got.outputs["double"], json!({ "result": 42 }));
    assert_eq!(got.next_node.as_deref(), Some("double"));

    // Orphan : Running et started_at < cutoff.
    let orphans = store.list_orphans(2_000).await.unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].run_id, "r1");
    assert!(store.list_orphans(1_000).await.unwrap().is_empty());

    // mark_failed pose status/error/finished_at ; inconnu → erreur.
    store.mark_failed("r1", "worker mort", 3_000).await.unwrap();
    let got = store.load("r1").await.unwrap().unwrap();
    assert_eq!(got.status, RunStatus::Failed);
    assert_eq!(got.error.as_deref(), Some("worker mort"));
    assert_eq!(got.finished_at_ms, Some(3_000));
    assert!(store.mark_failed("ghost", "x", 1).await.is_err());

    // list_by_workflow : tri created_at desc + total.
    store
        .checkpoint(&RunCheckpoint::queued("r2", "wf", 1, "manual", 2_000))
        .await
        .unwrap();
    let page = store
        .list_by_workflow(
            "wf",
            None,
            Pagination {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items[0].run_id, "r2"); // plus récent (created 2000) d'abord
    assert_eq!(page.items[1].run_id, "r1");

    // Propriétaire : persisté, immuable, et filtre de list_by_workflow.
    let owned =
        RunCheckpoint::queued("r3", "wf", 1, "manual", 3_000).with_owner(Some("alice".into()));
    store.checkpoint(&owned).await.unwrap();
    let mut later =
        RunCheckpoint::queued("r3", "wf", 1, "manual", 3_000).with_owner(Some("mallory".into()));
    later.status = RunStatus::Running;
    store.checkpoint(&later).await.unwrap();
    store
        .checkpoint(&RunCheckpoint::queued("r3", "wf", 1, "manual", 3_000))
        .await
        .unwrap();
    let r3 = store.load("r3").await.unwrap().unwrap();
    assert_eq!(
        r3.owner_id.as_deref(),
        Some("alice"),
        "propriétaire immuable"
    );

    let alice = store
        .list_by_workflow("wf", Some("alice"), Pagination::default())
        .await
        .unwrap();
    assert_eq!(alice.total, 1);
    assert_eq!(alice.items[0].run_id, "r3");
    let all = store
        .list_by_workflow("wf", None, Pagination::default())
        .await
        .unwrap();
    assert_eq!(all.total, 3);
}

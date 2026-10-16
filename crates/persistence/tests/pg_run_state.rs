//! Test d'intégration de [`PgRunStateStore`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Applique les migrations.

use autome_core::{Pagination, RunCheckpoint, RunStateStore, RunStatus};
use autome_persistence::{connect, run_migrations, PgRunStateStore};
use serde_json::json;

/// Chaque test repart d'un schéma vierge : on les sérialise (sinon un
/// `DROP SCHEMA` concurrent casserait le test voisin).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup() -> Option<(PgRunStateStore, tokio::sync::MutexGuard<'static, ()>)> {
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
    Some((PgRunStateStore::new(pool), guard))
}

#[tokio::test]
async fn checkpoint_load_orphans_failed_and_list() {
    let Some((store, _serial)) = setup().await else {
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

#[tokio::test(flavor = "multi_thread")]
async fn try_claim_has_a_single_winner_and_fail_if_running_is_conditional() {
    let Some((store, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let store = std::sync::Arc::new(store);
    store
        .checkpoint(
            &RunCheckpoint::queued("c1", "wf", 1, "manual", 1_000).with_owner(Some("alice".into())),
        )
        .await
        .unwrap();

    // 16 répliques reçoivent la même livraison en parallèle : un seul claim.
    let mut tasks = Vec::new();
    for i in 0..16 {
        let s = std::sync::Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            s.try_claim("c1", 2_000 + i).await.unwrap()
        }));
    }
    let mut claimed = Vec::new();
    for t in tasks {
        if let Some(cp) = t.await.unwrap() {
            claimed.push(cp);
        }
    }
    assert_eq!(claimed.len(), 1, "une seule réplique prend le run");
    assert_eq!(claimed[0].status, RunStatus::Running);
    assert_eq!(claimed[0].owner_id.as_deref(), Some("alice"));
    let started = claimed[0].started_at_ms;
    assert_eq!(
        store.load("c1").await.unwrap().unwrap().started_at_ms,
        started
    );
    assert!(store.try_claim("ghost", 1).await.unwrap().is_none());

    // Terminé : ni re-claim, ni écrasement par le balayage des orphelins.
    let mut done = store.load("c1").await.unwrap().unwrap();
    done.status = RunStatus::Succeeded;
    done.finished_at_ms = Some(3_000);
    store.checkpoint(&done).await.unwrap();
    assert!(store.try_claim("c1", 4_000).await.unwrap().is_none());
    assert!(!store
        .fail_if_running("c1", "orphelin", 4_000)
        .await
        .unwrap());
    assert_eq!(
        store.load("c1").await.unwrap().unwrap().status,
        RunStatus::Succeeded
    );

    // Running ⇒ Failed une seule fois.
    store
        .checkpoint(&RunCheckpoint::queued("c2", "wf", 1, "manual", 1_000))
        .await
        .unwrap();
    store.try_claim("c2", 1_500).await.unwrap().unwrap();
    assert!(store
        .fail_if_running("c2", "orphelin", 5_000)
        .await
        .unwrap());
    assert!(!store
        .fail_if_running("c2", "orphelin", 6_000)
        .await
        .unwrap());
    let failed = store.load("c2").await.unwrap().unwrap();
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("orphelin"));
    assert_eq!(failed.finished_at_ms, Some(5_000));
}

/// Écritures et lectures groupées (#198) : même sémantique qu'avant — la map
/// des sorties fait foi — avec un nombre d'instructions constant.
#[tokio::test]
async fn batched_outputs_keep_the_map_authoritative() {
    let Some((store, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    // Run large : 300 sorties écrites en un upsert.
    let mut big = RunCheckpoint::queued("big", "wf-big", 1, "manual", 1_000);
    for n in 0..300 {
        big.outputs.insert(format!("n{n:03}"), json!({ "n": n }));
    }
    store.checkpoint(&big).await.unwrap();
    assert_eq!(store.load("big").await.unwrap().unwrap(), big);

    // Checkpoint suivant : sorties modifiées, retirées et ajoutées.
    big.outputs
        .insert("n000".into(), json!({ "n": "modifiée" }));
    big.outputs.remove("n001");
    big.outputs.retain(|k, _| k.as_str() < "n100");
    big.outputs.insert("extra".into(), json!([1, 2, 3]));
    store.checkpoint(&big).await.unwrap();
    let got = store.load("big").await.unwrap().unwrap();
    assert_eq!(got, big, "modifiées, retirées et ajoutées");
    assert_eq!(got.outputs.len(), 100);

    // Map vide : toutes les sorties disparaissent.
    big.outputs.clear();
    store.checkpoint(&big).await.unwrap();
    assert!(store.load("big").await.unwrap().unwrap().outputs.is_empty());

    // Valeurs JSON variées, clés à caractères spéciaux.
    let mut odd = RunCheckpoint::queued("odd", "wf-odd", 1, "manual", 1_000);
    for (k, v) in [
        ("a'b", json!(null)),
        ("é \"q\"", json!("texte")),
        ("{x}", json!({ "nested": { "deep": [true, 1.5] } })),
    ] {
        odd.outputs.insert(k.into(), v);
    }
    store.checkpoint(&odd).await.unwrap();
    assert_eq!(store.load("odd").await.unwrap().unwrap(), odd);

    // Listes : chaque run reçoit ses propres sorties (lues en une requête),
    // runs sans sortie compris.
    for i in 0..30 {
        let mut cp = RunCheckpoint::queued(format!("l{i:02}"), "wf-list", 1, "manual", i);
        for n in 0..(i % 4) {
            cp.outputs
                .insert(format!("s{n}"), json!({ "run": i, "n": n }));
        }
        store.checkpoint(&cp).await.unwrap();
    }
    let page = store
        .list_by_workflow(
            "wf-list",
            None,
            Pagination {
                offset: 5,
                limit: 20,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 30);
    assert_eq!(page.items.len(), 20);
    for run in &page.items {
        let i: i64 = run.run_id[1..].parse().unwrap();
        assert_eq!(run.outputs.len() as i64, i % 4, "{}", run.run_id);
        assert!(
            run.outputs.values().all(|v| v["run"] == json!(i)),
            "sorties d'un autre run : {}",
            run.run_id
        );
    }

    // Orphelins : idem, plusieurs runs avec leurs sorties.
    for i in 0..5 {
        let mut cp = RunCheckpoint::queued(format!("o{i}"), "wf-orph", 1, "manual", 1);
        cp.status = RunStatus::Running;
        cp.started_at_ms = Some(10);
        cp.outputs.insert("only".into(), json!(i));
        store.checkpoint(&cp).await.unwrap();
    }
    let orphans = store.list_orphans(100).await.unwrap();
    assert_eq!(orphans.len(), 5);
    for (i, run) in orphans.iter().enumerate() {
        assert_eq!(run.run_id, format!("o{i}"));
        assert_eq!(run.outputs["only"], json!(i));
    }

    // Claim : renvoie le run avec ses sorties.
    let mut queued = RunCheckpoint::queued("q", "wf-q", 1, "manual", 1);
    queued.outputs.insert("seed".into(), json!("payload"));
    store.checkpoint(&queued).await.unwrap();
    let claimed = store.try_claim("q", 50).await.unwrap().unwrap();
    assert_eq!(claimed.outputs["seed"], json!("payload"));
}

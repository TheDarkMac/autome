//! Test d'intégration de [`PgTriggerScheduleStore`] contre un vrai PostgreSQL
//! (#227). Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Applique les
//! migrations réelles.

use std::sync::Arc;

use autome_core::{
    Node, NodeId, NodeKind, ScheduledTrigger, SlotUpdate, TriggerScheduleStore, WorkflowGraph,
    WorkflowId, WorkflowRepository,
};
use autome_persistence::{
    connect, run_migrations, PgPool, PgTriggerScheduleStore, PgWorkflowRepository,
};
use serde_json::{json, Value};

fn trigger(id: &str, trigger_type: &str, params: Value) -> Node {
    Node {
        position: None,
        id: NodeId::new(id),
        name: id.into(),
        kind: NodeKind::Trigger {
            trigger_type: trigger_type.into(),
        },
        params,
    }
}

fn wf(id: &str, nodes: Vec<Node>) -> WorkflowGraph {
    WorkflowGraph {
        id: WorkflowId::new(id),
        name: id.into(),
        nodes,
        edges: vec![],
    }
}

fn cron(node: &str, expr: &str) -> ScheduledTrigger {
    ScheduledTrigger {
        node_id: node.into(),
        trigger_type: "schedule".into(),
        params: json!({ "cron": expr }),
    }
}

fn at(next: i64) -> SlotUpdate {
    SlotUpdate {
        next_fire_ms: Some(next),
        state: json!({}),
        error: None,
    }
}

async fn setup() -> Option<(String, PgPool)> {
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
    Some((url, pool))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedules_lifecycle_and_backfill() {
    let Some((url, pool)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let repo = PgWorkflowRepository::new(pool.clone());
    let store = PgTriggerScheduleStore::new(pool.clone());
    repo.create("alice", wf("wf", vec![])).await.unwrap();
    repo.create("bob", wf("wf", vec![])).await.unwrap();

    // Workflow inexistant : refusé par la clé étrangère.
    assert!(store
        .sync("alice", "ghost", &[cron("t", "* * * * *")])
        .await
        .is_err());

    // Création : échéance à calculer, état vide.
    store
        .sync(
            "alice",
            "wf",
            &[cron("a", "0 * * * *"), cron("b", "0 * * * *")],
        )
        .await
        .unwrap();
    store
        .sync("bob", "wf", &[cron("a", "0 * * * *")])
        .await
        .unwrap();
    let due = store.due(0, 10).await.unwrap();
    assert_eq!(due.len(), 3);
    assert!(due
        .iter()
        .all(|s| s.next_fire_ms.is_none() && s.state == json!({})));

    // Compare-and-set concurrent entre deux répliques : une seule gagne.
    let other = Arc::new(PgTriggerScheduleStore::new(connect(&url).expect("pool b")));
    let store = Arc::new(store);
    let slot = due
        .iter()
        .find(|s| s.owner_id == "alice" && s.node_id == "a")
        .unwrap()
        .clone();
    let mut tasks = Vec::new();
    for i in 0..8 {
        let s: Arc<dyn TriggerScheduleStore> = if i % 2 == 0 {
            store.clone()
        } else {
            other.clone()
        };
        let slot = slot.clone();
        tasks.push(tokio::spawn(async move {
            s.advance(&slot, at(3_600_000)).await.unwrap()
        }));
    }
    let mut won = 0;
    for t in tasks {
        won += usize::from(t.await.unwrap());
    }
    assert_eq!(won, 1, "une seule réplique avance une révision donnée");

    // Échéance future : plus due ; atteinte : due, révision suivante.
    let pending: Vec<_> = store.due(3_599_999, 10).await.unwrap();
    assert!(pending
        .iter()
        .all(|s| !(s.owner_id == "alice" && s.node_id == "a")));
    let a = store
        .due(3_600_000, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.owner_id == "alice" && s.node_id == "a")
        .unwrap();
    assert_eq!(
        (a.next_fire_ms, a.revision),
        (Some(3_600_000), slot.revision + 1)
    );
    // Ordre : à calculer d'abord.
    assert_eq!(
        store
            .due(3_600_000, 10)
            .await
            .unwrap()
            .last()
            .unwrap()
            .node_id,
        "a"
    );

    // État et erreur persistés.
    let state = json!({ "seen": ["1", "2"] });
    assert!(store
        .advance(
            &a,
            SlotUpdate {
                state: state.clone(),
                ..at(7_200_000)
            }
        )
        .await
        .unwrap());

    // Sync : `a` inchangé (état et révision gardés), `b` modifié (remis à
    // zéro), `c` ajouté ; un nœud retiré disparaît.
    store
        .sync(
            "alice",
            "wf",
            &[cron("a", "0 * * * *"), cron("b", "30 * * * *")],
        )
        .await
        .unwrap();
    let after = store.due(i64::MAX, 10).await.unwrap();
    let alice = |node: &str| {
        after
            .iter()
            .find(|s| s.owner_id == "alice" && s.node_id == node)
            .cloned()
    };
    let a2 = alice("a").unwrap();
    assert_eq!((a2.state.clone(), a2.revision), (state, a.revision + 1));
    let b = alice("b").unwrap();
    assert_eq!((b.next_fire_ms, b.revision), (None, 1));
    let stale_b = due
        .iter()
        .find(|s| s.owner_id == "alice" && s.node_id == "b")
        .unwrap();
    assert!(
        !store.advance(stale_b, at(1)).await.unwrap(),
        "révision périmée"
    );

    store
        .sync("alice", "wf", &[cron("a", "0 * * * *")])
        .await
        .unwrap();
    assert!(store
        .due(i64::MAX, 10)
        .await
        .unwrap()
        .iter()
        .all(|s| !(s.owner_id == "alice" && s.node_id == "b")));

    // Nœud en erreur : plus jamais dû jusqu'à sa modification.
    let a3 = store
        .due(i64::MAX, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.owner_id == "alice" && s.node_id == "a")
        .unwrap();
    assert!(store
        .advance(
            &a3,
            SlotUpdate {
                next_fire_ms: None,
                state: json!({}),
                error: Some("cron invalide".into()),
            }
        )
        .await
        .unwrap());
    let still = store.due(i64::MAX, 10).await.unwrap();
    assert!(still.iter().all(|s| s.owner_id != "alice"));
    store
        .sync("alice", "wf", &[cron("a", "5 * * * *")])
        .await
        .unwrap();
    assert!(store
        .due(0, 10)
        .await
        .unwrap()
        .iter()
        .any(|s| s.owner_id == "alice" && s.node_id == "a"));

    // Suppression du workflow : cascade.
    assert!(repo.delete("alice", &WorkflowId::new("wf")).await.unwrap());
    assert!(store
        .due(i64::MAX, 10)
        .await
        .unwrap()
        .iter()
        .all(|s| s.owner_id != "alice"));

    // ── Reprise des workflows existants par la migration 0019 ──
    repo.create(
        "carol",
        wf(
            "legacy",
            vec![
                trigger("cron", "schedule", json!({ "cron": "0 9 * * 1-5" })),
                trigger("poll", "polling", json!({ "url": "https://x" })),
                trigger("hook", "webhook", json!({})),
                trigger("bare", "schedule", Value::Null),
            ],
        ),
    )
    .await
    .unwrap();
    {
        let client = pool.get().await.unwrap();
        client
            .batch_execute(
                "DROP TABLE trigger_schedules; \
                 DELETE FROM schema_migrations WHERE name = '0019_trigger_schedules.sql';",
            )
            .await
            .unwrap();
    }
    assert_eq!(run_migrations(&pool).await.unwrap(), 1);
    let mut backfilled: Vec<_> = store
        .due(0, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|s| {
            (
                s.owner_id,
                s.node_id,
                s.trigger_type,
                s.params,
                s.next_fire_ms,
            )
        })
        .collect();
    backfilled.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        backfilled,
        vec![
            (
                "carol".into(),
                "bare".into(),
                "schedule".into(),
                Value::Null,
                None
            ),
            (
                "carol".into(),
                "cron".into(),
                "schedule".into(),
                json!({ "cron": "0 9 * * 1-5" }),
                None
            ),
            (
                "carol".into(),
                "poll".into(),
                "polling".into(),
                json!({ "url": "https://x" }),
                None
            ),
        ]
    );
}

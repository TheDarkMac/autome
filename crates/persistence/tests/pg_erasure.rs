//! Test d'intégration de [`PgAccountEraser`] contre un vrai PostgreSQL (#229).
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Applique les migrations.

use std::collections::HashSet;

use autome_connectors::oauth::{ConnectorTokenStore, TokenSet};
use autome_core::{
    AccountEraser, AuditEvent, AuditOutcome, AuditSink, CreateUser, EndpointSecret, EnvelopeCipher,
    ErasureReport, InMemoryKeyProvider, Node, NodeId, NodeKind, PermissionStore, RunCheckpoint,
    RunStateStore, ScheduledTrigger, TriggerScheduleStore, UserStore, WebhookEndpointStore,
    WorkflowGraph, WorkflowId, WorkflowRepository,
};
use autome_persistence::{
    connect, run_migrations, PgAccountEraser, PgAuditSink, PgConnectorTokenStore,
    PgPermissionStore, PgPool, PgRunStateStore, PgTriggerScheduleStore, PgUserStore,
    PgWebhookEndpointStore, PgWorkflowRepository,
};
use serde_json::{json, Value};

/// Chaque test repart d'un schéma vierge : on les sérialise.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup() -> Option<(PgPool, tokio::sync::MutexGuard<'static, ()>)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let guard = SERIAL.lock().await;
    let pool = connect(&url).expect("pool");
    pool.get()
        .await
        .expect("conn")
        .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .await
        .expect("reset");
    run_migrations(&pool).await.expect("migrations");
    Some((pool, guard))
}

fn node(id: &str, trigger_type: &str, params: Value) -> Node {
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

fn wf(id: &str) -> WorkflowGraph {
    WorkflowGraph {
        id: WorkflowId::new(id),
        name: id.into(),
        nodes: vec![
            node("hook", "webhook", Value::Null),
            node("tick", "schedule", json!({ "interval_seconds": 60 })),
        ],
        edges: vec![],
    }
}

async fn count(pool: &PgPool, sql: &str, owner: &str) -> i64 {
    pool.get()
        .await
        .unwrap()
        .query_one(sql, &[&owner])
        .await
        .unwrap()
        .get(0)
}

/// Compte `email` avec un workflow (webhook + échéance), un run terminé avec
/// sorties, un token Slack et des événements d'audit (dont un sous l'email).
async fn seed(pool: &PgPool, email: &str) -> String {
    let users = PgUserStore::new(pool.clone());
    let perms: HashSet<String> = ["workflows.read".to_string()].into();
    let CreateUser::Created(user) = users.create(email, "hash", &perms).await.unwrap() else {
        panic!("compte");
    };
    let id = user.id;

    PgWorkflowRepository::new(pool.clone())
        .create(&id, wf("wf-1"))
        .await
        .unwrap();
    PgWebhookEndpointStore::new(pool.clone())
        .ensure(
            &id,
            "wf-1",
            "hook",
            EndpointSecret {
                salt: vec![email.len() as u8; 32],
                // Unique par compte (colonne `UNIQUE`).
                token_hash: vec![email.as_bytes()[0]; 32],
            },
        )
        .await
        .unwrap();
    PgTriggerScheduleStore::new(pool.clone())
        .sync(
            &id,
            "wf-1",
            &[ScheduledTrigger {
                node_id: "tick".into(),
                trigger_type: "schedule".into(),
                params: json!({ "interval_seconds": 60 }),
            }],
        )
        .await
        .unwrap();

    let runs = PgRunStateStore::new(pool.clone());
    let mut run = RunCheckpoint::queued(format!("run-{id}"), "wf-1", 1, "manual", 1_000)
        .with_owner(Some(id.clone()));
    runs.checkpoint(&run).await.unwrap();
    run.outputs
        .insert("hook".into(), json!({ "customer": "alice@client.io" }));
    runs.checkpoint(&run).await.unwrap();

    let tokens = PgConnectorTokenStore::new(
        pool.clone(),
        EnvelopeCipher::new(InMemoryKeyProvider::with_key("v1", [7u8; 32])),
    );
    let set = TokenSet {
        access_token: "xoxb".into(),
        refresh_token: None,
        expires_at_ms: None,
        token_type: "Bearer".into(),
        scope: None,
    };
    tokio::task::block_in_place(|| tokens.store(&id, "slack", &set)).unwrap();

    let audit = PgAuditSink::new(pool.clone());
    for event in [
        AuditEvent::new(1, email, "auth.login", AuditOutcome::Failure),
        AuditEvent::new(2, id.as_str(), "auth.login", AuditOutcome::Success),
        AuditEvent::new(3, "admin", "admin.user_disable", AuditOutcome::Success)
            .with_resource(id.as_str()),
    ] {
        audit.record(event).await.unwrap();
    }
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn erase_removes_everything_of_the_account_and_nothing_else() {
    let Some((pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let alice = seed(&pool, "alice@example.io").await;
    let bob = seed(&pool, "bob@example.io").await;
    let eraser = PgAccountEraser::new(pool.clone());

    let report = eraser.erase(&alice, "erased:0011").await.unwrap();
    assert_eq!(
        report,
        Some(ErasureReport {
            workflows: 1,
            runs: 1,
            connector_tokens: 1,
            audit_events: 3,
        })
    );

    for (sql, expected_alice) in [
        ("SELECT count(*) FROM users WHERE id::text = $1", 0),
        (
            "SELECT count(*) FROM user_permissions WHERE user_id = $1",
            0,
        ),
        ("SELECT count(*) FROM workflows WHERE owner_id = $1", 0),
        (
            "SELECT count(*) FROM workflow_versions WHERE owner_id = $1",
            0,
        ),
        (
            "SELECT count(*) FROM webhook_endpoints WHERE owner_id = $1",
            0,
        ),
        (
            "SELECT count(*) FROM trigger_schedules WHERE owner_id = $1",
            0,
        ),
        ("SELECT count(*) FROM runs WHERE owner_id = $1", 0),
        (
            "SELECT count(*) FROM run_steps s JOIN runs r ON r.id = s.run_id WHERE r.owner_id = $1",
            0,
        ),
        (
            "SELECT count(*) FROM connector_tokens WHERE user_id = $1",
            0,
        ),
        (
            "SELECT count(*) FROM audit_events WHERE actor = $1 OR resource = $1",
            0,
        ),
    ] {
        assert_eq!(count(&pool, sql, &alice).await, expected_alice, "{sql}");
        assert!(count(&pool, sql, &bob).await >= 1, "bob intact : {sql}");
    }
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM run_steps WHERE run_id = $1",
            &format!("run-{alice}")
        )
        .await,
        0,
        "sorties des nœuds effacées"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM audit_events WHERE actor = $1",
            "alice@example.io"
        )
        .await,
        0,
        "email retiré de l'audit"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM audit_events WHERE actor = $1 OR resource = $1",
            "erased:0011"
        )
        .await,
        3,
        "événements gardés sous le pseudonyme"
    );
    assert!(PgPermissionStore::new(pool.clone())
        .permissions_for(&alice)
        .await
        .unwrap()
        .is_empty());

    // Déjà effacé, inconnu ou mal formé : rien.
    assert_eq!(eraser.erase(&alice, "erased:22").await.unwrap(), None);
    assert_eq!(
        eraser.erase("pas-un-uuid", "erased:22").await.unwrap(),
        None
    );
    // L'email redevient libre.
    assert!(matches!(
        PgUserStore::new(pool.clone())
            .create("alice@example.io", "h", &HashSet::new())
            .await
            .unwrap(),
        CreateUser::Created(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_step_rolls_the_whole_erasure_back() {
    let Some((pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let alice = seed(&pool, "alice@example.io").await;
    // Dernière étape (suppression du compte) en échec.
    pool.get()
        .await
        .unwrap()
        .batch_execute(
            "CREATE FUNCTION refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'refusé'; END $$; \
             CREATE TRIGGER refuse BEFORE DELETE ON users FOR EACH ROW EXECUTE FUNCTION refuse();",
        )
        .await
        .unwrap();

    assert!(PgAccountEraser::new(pool.clone())
        .erase(&alice, "erased:33")
        .await
        .is_err());
    for sql in [
        "SELECT count(*) FROM users WHERE id::text = $1",
        "SELECT count(*) FROM workflows WHERE owner_id = $1",
        "SELECT count(*) FROM runs WHERE owner_id = $1",
        "SELECT count(*) FROM connector_tokens WHERE user_id = $1",
        "SELECT count(*) FROM user_permissions WHERE user_id = $1",
        "SELECT count(*) FROM audit_events WHERE actor = $1",
    ] {
        assert!(count(&pool, sql, &alice).await >= 1, "rien effacé : {sql}");
    }
}

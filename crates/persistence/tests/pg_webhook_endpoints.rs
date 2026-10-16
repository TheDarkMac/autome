//! Test d'intégration de [`PgWebhookEndpointStore`] contre un vrai
//! PostgreSQL (#202). Gaté par `TEST_DATABASE_URL` (skip-pass sinon).
//! Applique les migrations réelles.

use std::sync::Arc;

use autome_core::{
    EndpointSecret, Node, NodeId, NodeKind, WebhookEndpoint, WebhookEndpointStore, WorkflowGraph,
    WorkflowId, WorkflowRepository,
};
use autome_persistence::{connect, run_migrations, PgWebhookEndpointStore, PgWorkflowRepository};
use serde_json::Value;

fn wf(id: &str) -> WorkflowGraph {
    WorkflowGraph {
        id: WorkflowId::new(id),
        name: id.into(),
        nodes: vec![Node {
            position: None,
            id: NodeId::new("hook"),
            name: "hook".into(),
            kind: NodeKind::Trigger {
                trigger_type: "webhook".into(),
            },
            params: Value::Null,
        }],
        edges: vec![],
    }
}

fn secret(byte: u8) -> EndpointSecret {
    EndpointSecret {
        salt: vec![byte; 32],
        token_hash: vec![byte.wrapping_add(100); 32],
    }
}

/// Deux stores sur deux pools distincts (deux répliques de l'API) + le
/// repository des workflows.
async fn setup() -> Option<(
    PgWebhookEndpointStore,
    PgWebhookEndpointStore,
    PgWorkflowRepository,
)> {
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
    Some((
        PgWebhookEndpointStore::new(pool.clone()),
        PgWebhookEndpointStore::new(connect(&url).expect("pool b")),
        PgWorkflowRepository::new(pool),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endpoints_lifecycle() {
    let Some((a, b, repo)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    repo.create("alice", wf("wf")).await.unwrap();
    repo.create("alice", wf("other")).await.unwrap();

    // Endpoint d'un workflow inexistant : refusé par la clé étrangère.
    assert!(a.ensure("alice", "ghost", "hook", secret(9)).await.is_err());

    // Créations concurrentes sur deux répliques : un seul sel gagne.
    let (a, b) = (Arc::new(a), Arc::new(b));
    let mut tasks = Vec::new();
    for i in 0..10u8 {
        let store = if i % 2 == 0 {
            Arc::clone(&a)
        } else {
            Arc::clone(&b)
        };
        tasks.push(tokio::spawn(async move {
            store
                .ensure("alice", "wf", "hook", secret(i + 1))
                .await
                .unwrap()
        }));
    }
    let mut salts = Vec::new();
    for t in tasks {
        salts.push(t.await.unwrap());
    }
    salts.dedup();
    assert_eq!(salts.len(), 1, "un seul secret pour l'endpoint");
    let winner = salts[0][0];
    let target = a
        .resolve(&secret(winner).token_hash)
        .await
        .unwrap()
        .expect("résolu");
    assert_eq!(
        target,
        WebhookEndpoint {
            owner_id: "alice".into(),
            workflow_id: "wf".into(),
            node_id: "hook".into(),
            salt: vec![winner; 32],
        }
    );

    // Rotation : l'ancien token ne résout plus, le nouveau oui.
    b.rotate("alice", "wf", "hook", secret(50)).await.unwrap();
    assert!(a
        .resolve(&secret(winner).token_hash)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        a.resolve(&secret(50).token_hash)
            .await
            .unwrap()
            .unwrap()
            .salt,
        vec![50; 32]
    );
    // `ensure` ne revient pas sur une rotation.
    assert_eq!(
        a.ensure("alice", "wf", "hook", secret(60)).await.unwrap(),
        vec![50; 32]
    );

    // Purge des nœuds retirés, limitée au workflow visé.
    a.ensure("alice", "wf", "gone", secret(70)).await.unwrap();
    a.ensure("alice", "other", "hook", secret(80))
        .await
        .unwrap();
    assert_eq!(a.retain("alice", "wf", &["hook".into()]).await.unwrap(), 1);
    assert!(a.resolve(&secret(70).token_hash).await.unwrap().is_none());
    assert!(a.resolve(&secret(50).token_hash).await.unwrap().is_some());

    // Suppression du workflow : ses endpoints partent en cascade.
    assert!(repo.delete("alice", &WorkflowId::new("wf")).await.unwrap());
    assert!(a.resolve(&secret(50).token_hash).await.unwrap().is_none());
    assert!(a.resolve(&secret(80).token_hash).await.unwrap().is_some());
}

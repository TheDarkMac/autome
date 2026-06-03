//! Test d'intégration de [`PgWorkflowRepository`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Applique les migrations.

use autome_core::{
    Edge, Node, NodeId, NodeKind, Pagination, WorkflowGraph, WorkflowId, WorkflowRepository,
};
use autome_persistence::{connect, run_migrations, PgWorkflowRepository};
use serde_json::Value;

fn wf(id: &str, name: &str) -> WorkflowGraph {
    WorkflowGraph {
        id: WorkflowId::new(id),
        name: name.into(),
        nodes: vec![Node {
            id: NodeId::new("entry"),
            name: "entry".into(),
            kind: NodeKind::Trigger {
                trigger_type: "manual".into(),
            },
            params: Value::Null,
        }],
        edges: vec![Edge {
            from: NodeId::new("entry"),
            to: NodeId::new("entry"),
            from_port: None,
        }],
    }
}

async fn setup() -> Option<PgWorkflowRepository> {
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
    Some(PgWorkflowRepository::new(pool))
}

#[tokio::test]
async fn crud_roundtrip() {
    let Some(repo) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    // Inconnu.
    assert!(repo.get(&WorkflowId::new("a")).await.unwrap().is_none());

    // Create + get.
    repo.create(wf("a", "Alpha")).await.unwrap();
    let got = repo.get(&WorkflowId::new("a")).await.unwrap().unwrap();
    assert_eq!(got.name, "Alpha");
    assert_eq!(got.nodes.len(), 1);

    // Doublon rejeté.
    assert!(repo.create(wf("a", "dup")).await.is_err());

    // Update (nouvelle version, get renvoie la courante).
    repo.update(wf("a", "Alpha v2")).await.unwrap();
    assert_eq!(
        repo.get(&WorkflowId::new("a")).await.unwrap().unwrap().name,
        "Alpha v2"
    );
    // Update d'un inconnu → erreur.
    assert!(repo.update(wf("ghost", "x")).await.is_err());

    // List paginée.
    repo.create(wf("b", "Beta")).await.unwrap();
    let page = repo
        .list(Pagination {
            offset: 0,
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 2);

    // Delete (idempotent).
    assert!(repo.delete(&WorkflowId::new("a")).await.unwrap());
    assert!(!repo.delete(&WorkflowId::new("a")).await.unwrap());
    assert!(repo.get(&WorkflowId::new("a")).await.unwrap().is_none());
}

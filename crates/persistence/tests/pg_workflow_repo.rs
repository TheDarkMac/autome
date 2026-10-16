//! Test d'intégration de [`PgWorkflowRepository`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Applique les migrations.

use std::sync::Arc;

use autome_core::{
    Edge, Node, NodeId, NodeKind, Pagination, WorkflowGraph, WorkflowId, WorkflowRepository,
    LEGACY_NAMESPACE,
};
use autome_persistence::{connect, run_migrations, PgWorkflowRepository};
use serde_json::Value;

fn wf(id: &str, name: &str) -> WorkflowGraph {
    WorkflowGraph {
        id: WorkflowId::new(id),
        name: name.into(),
        nodes: vec![Node {
            position: None,
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

fn id(s: &str) -> WorkflowId {
    WorkflowId::new(s)
}

const PAGING: Pagination = Pagination {
    offset: 0,
    limit: 10,
};

#[tokio::test]
async fn crud_namespaces_and_concurrent_updates() {
    let Some(repo) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    // Inconnu.
    assert!(repo.get("alice", &id("a")).await.unwrap().is_none());

    // Create + get.
    repo.create("alice", wf("a", "Alpha")).await.unwrap();
    let got = repo.get("alice", &id("a")).await.unwrap().unwrap();
    assert_eq!(got.name, "Alpha");
    assert_eq!(got.nodes.len(), 1);

    // Doublon rejeté dans le même espace seulement (#188) : le même id est
    // libre dans l'espace de bob.
    let dup = repo.create("alice", wf("a", "dup")).await.unwrap_err();
    assert!(dup.to_string().contains("déjà"));
    repo.create("bob", wf("a", "Alpha de bob")).await.unwrap();
    assert_eq!(
        repo.get("alice", &id("a")).await.unwrap().unwrap().name,
        "Alpha"
    );
    assert_eq!(
        repo.get("bob", &id("a")).await.unwrap().unwrap().name,
        "Alpha de bob"
    );

    // Update (nouvelle version, get renvoie la courante), confiné à l'espace.
    repo.update("alice", wf("a", "Alpha v2")).await.unwrap();
    assert_eq!(
        repo.get("alice", &id("a")).await.unwrap().unwrap().name,
        "Alpha v2"
    );
    assert_eq!(
        repo.get("bob", &id("a")).await.unwrap().unwrap().name,
        "Alpha de bob"
    );
    assert!(repo.update("alice", wf("ghost", "x")).await.is_err());
    assert!(repo.update("carol", wf("a", "x")).await.is_err());

    // List paginée par espace ; espace des workflows sans propriétaire.
    repo.create("bob", wf("b", "Beta")).await.unwrap();
    repo.create(LEGACY_NAMESPACE, wf("c", "Legacy"))
        .await
        .unwrap();
    let alice = repo.list("alice", PAGING).await.unwrap();
    assert_eq!(alice.total, 1);
    assert_eq!(alice.items[0].name, "Alpha v2");
    let bob = repo.list("bob", PAGING).await.unwrap();
    assert_eq!(bob.total, 2);
    let ids: Vec<&str> = bob.items.iter().map(|w| w.id.0.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
    assert_eq!(repo.list(LEGACY_NAMESPACE, PAGING).await.unwrap().total, 1);
    assert_eq!(repo.list("nobody", PAGING).await.unwrap().total, 0);

    // Mises à jour concurrentes du même workflow : toutes passent, chacune
    // publie sa version (avant le verrou de ligne : conflit de clé ⇒ erreur).
    let repo = Arc::new(repo);
    let tasks: Vec<_> = (0..10)
        .map(|i| {
            let repo = repo.clone();
            tokio::spawn(async move { repo.update("bob", wf("b", &format!("Beta {i}"))).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().expect("mise à jour concurrente");
    }

    // Delete (idempotent), confiné à l'espace.
    assert!(!repo.delete("carol", &id("a")).await.unwrap());
    assert!(repo.delete("alice", &id("a")).await.unwrap());
    assert!(!repo.delete("alice", &id("a")).await.unwrap());
    assert!(repo.get("alice", &id("a")).await.unwrap().is_none());
    assert!(repo.get("bob", &id("a")).await.unwrap().is_some());
}

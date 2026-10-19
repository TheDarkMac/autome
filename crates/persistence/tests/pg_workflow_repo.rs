//! Test d'intégration de [`PgWorkflowRepository`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Applique les migrations.

use std::sync::Arc;

use autome_core::{
    Edge, Node, NodeId, NodeKind, Pagination, WorkflowFilter, WorkflowGraph, WorkflowId,
    WorkflowListingPage, WorkflowRepository, LEGACY_NAMESPACE,
};
use autome_persistence::{connect, run_migrations, PgWorkflowRepository};
use serde_json::Value;
use tokio::sync::MutexGuard;

/// Les tests réinitialisent le schéma : un seul à la fois.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

async fn setup() -> Option<(PgWorkflowRepository, MutexGuard<'static, ()>)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let serial = SERIAL.lock().await;
    let pool = connect(&url).expect("pool");
    {
        let client = pool.get().await.expect("conn");
        client
            .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .await
            .expect("reset");
    }
    run_migrations(&pool).await.expect("migrations");
    Some((PgWorkflowRepository::new(pool), serial))
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
    let Some((repo, _serial)) = setup().await else {
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

fn listed(page: &WorkflowListingPage) -> Vec<(&str, &str, &str, usize)> {
    page.items
        .iter()
        .map(|w| {
            (
                w.owner.as_str(),
                w.id.0.as_str(),
                w.name.as_str(),
                w.node_count,
            )
        })
        .collect()
}

#[tokio::test]
async fn admin_listing_spans_namespaces_with_filters_and_paging() {
    let Some((repo, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    repo.create("bob", wf("b", "Factures")).await.unwrap();
    repo.create("alice", wf("z", "Zeta")).await.unwrap();
    repo.create("alice", wf("a", "Alpha")).await.unwrap();
    repo.create(LEGACY_NAMESPACE, wf("old", "Ancien"))
        .await
        .unwrap();
    // Le nom et le nombre de nœuds suivent la version courante.
    let mut renamed = wf("a", "Alpha 100% _v2_");
    renamed.nodes.push(Node {
        position: None,
        id: NodeId::new("log"),
        name: "log".into(),
        kind: NodeKind::Action {
            action_type: "log".into(),
        },
        params: Value::Null,
    });
    repo.update("alice", renamed).await.unwrap();

    let all = repo
        .list_all(&WorkflowFilter::default(), PAGING)
        .await
        .unwrap();
    assert_eq!(all.total, 4);
    assert_eq!(
        listed(&all),
        vec![
            ("", "old", "Ancien", 1),
            ("alice", "a", "Alpha 100% _v2_", 2),
            ("alice", "z", "Zeta", 1),
            ("bob", "b", "Factures", 1),
        ]
    );

    let filter = |owner: Option<&str>, query: Option<&str>| WorkflowFilter {
        owner: owner.map(Into::into),
        query: query.map(Into::into),
    };
    // Propriétaire exact, espace legacy compris.
    let alice = repo
        .list_all(&filter(Some("alice"), None), PAGING)
        .await
        .unwrap();
    assert_eq!(alice.total, 2);
    let legacy = repo
        .list_all(&filter(Some(LEGACY_NAMESPACE), None), PAGING)
        .await
        .unwrap();
    assert_eq!(listed(&legacy), vec![("", "old", "Ancien", 1)]);
    assert_eq!(
        repo.list_all(&filter(Some("ali"), None), PAGING)
            .await
            .unwrap()
            .total,
        0
    );

    // Recherche dans l'id ou le nom, sans casse ; `%` et `_` sont littéraux.
    let search = |q: &'static str| {
        let repo = &repo;
        async move {
            let page = repo.list_all(&filter(None, Some(q)), PAGING).await.unwrap();
            page.items
                .iter()
                .map(|w| format!("{}/{}", w.owner, w.id.0))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(search(" FACT ").await, vec!["bob/b"]);
    assert_eq!(search("OLD").await, vec!["/old"]);
    assert_eq!(search("100%").await, vec!["alice/a"]);
    assert!(search("1_0").await.is_empty());
    let both = repo
        .list_all(&filter(Some("alice"), Some("zeta")), PAGING)
        .await
        .unwrap();
    assert_eq!(listed(&both), vec![("alice", "z", "Zeta", 1)]);

    // Pagination, total du filtre.
    let page = repo
        .list_all(
            &WorkflowFilter::default(),
            Pagination {
                offset: 1,
                limit: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 4);
    let ids: Vec<&str> = page.items.iter().map(|w| w.id.0.as_str()).collect();
    assert_eq!(ids, vec!["a", "z"]);
}

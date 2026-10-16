//! Résolution des tokens de connecteurs **par propriétaire du run** contre un
//! **vrai** PostgreSQL : le token écrit dans `connector_tokens` (comme le fait
//! l'API au callback OAuth) est relu et déchiffré par le worker pour le seul
//! propriétaire du run.
//!
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon) :
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=pg -p 5440:5432 postgres:16-alpine
//! TEST_DATABASE_URL=postgres://postgres:pg@localhost:5440/postgres \
//!   cargo test -p autome-worker --test pg_owner_credentials
//! ```

use std::sync::{Arc, Mutex};

use autome_connectors::oauth::{RefreshError, TokenRefresher, TokenSet};
use autome_connectors::{HttpClient, HttpRequest, HttpResponse};
use autome_core::{
    Config, Delivery, DeliveryId, Edge, InMemoryRunStateStore, InMemoryWorkflowRepository, Message,
    Node, NodeId, NodeKind, RunCheckpoint, RunStateStore, RunStatus, WorkflowGraph, WorkflowId,
    WorkflowRepository,
};
use autome_persistence::{build_connector_token_store, connect, PersistenceMode, MIGRATIONS};
use autome_worker::{ConnectorCredentialsMode, Handler, HandlerVerdict, RunExecutor};
use serde_json::json;

/// Refresher qui ne doit jamais être appelé (tokens non expirants).
struct NoRefresh;
impl TokenRefresher for NoRefresh {
    fn refresh(&self, _: &str, _: &str, _: i64) -> Result<TokenSet, RefreshError> {
        panic!("aucun rafraîchissement attendu");
    }
}

/// Refresher de test : renvoie un nouvel access token, sans refresh token
/// (cas Google) — l'ancien doit être conservé en base.
struct FakeGoogle;
impl TokenRefresher for FakeGoogle {
    fn refresh(
        &self,
        provider: &str,
        refresh_token: &str,
        now: i64,
    ) -> Result<TokenSet, RefreshError> {
        assert_eq!((provider, refresh_token), ("gmail", "rt-alice"));
        Ok(TokenSet {
            access_token: "ya29-refreshed".into(),
            refresh_token: None,
            expires_at_ms: Some(now + 3_600_000),
            token_type: "Bearer".into(),
            scope: None,
        })
    }
}

/// Transport qui enregistre les requêtes (aucun appel réseau vers GitHub).
#[derive(Clone, Default)]
struct RecordingHttp {
    sent: Arc<Mutex<Vec<HttpRequest>>>,
}

impl HttpClient for RecordingHttp {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        self.sent.lock().unwrap().push(req.clone());
        Ok(HttpResponse {
            status: 201,
            body: json!({ "number": 1 }),
        })
    }
}

fn github_workflow(id: &str) -> WorkflowGraph {
    WorkflowGraph {
        id: WorkflowId::new(id),
        name: id.into(),
        nodes: vec![
            Node {
                id: NodeId::new("entry"),
                name: "entry".into(),
                kind: NodeKind::Trigger {
                    trigger_type: "manual".into(),
                },
                params: serde_json::Value::Null,
            },
            Node {
                id: NodeId::new("issue"),
                name: "issue".into(),
                kind: NodeKind::Action {
                    action_type: "github.create_issue".into(),
                },
                params: json!({ "owner": "o", "repo": "r", "title": "t" }),
            },
        ],
        edges: vec![Edge {
            from: NodeId::new("entry"),
            to: NodeId::new("issue"),
            from_port: None,
        }],
    }
}

fn delivery(run_id: &str, workflow_id: &str) -> Delivery {
    Delivery {
        id: DeliveryId::new(1),
        queue: "runs".into(),
        message: Message::new(json!({ "run_id": run_id, "workflow_id": workflow_id })),
        redeliveries: 0,
    }
}

/// Config + table `connector_tokens` neuve (DDL réel des migrations 0011 et 0018).
/// Les tests du binaire sont sérialisés : chacun recrée la table.
async fn setup() -> Option<(Config, tokio::sync::MutexGuard<'static, ()>)> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let guard = SERIAL.lock().await;
    let config = Config::load(move |k| match k {
        "DATABASE_URL" => Some(url.clone()),
        "ENCRYPTION_KEY" => Some("worker-test-encryption-key".into()),
        _ => None,
    })
    .unwrap();
    let ddl: String = MIGRATIONS
        .iter()
        .filter(|(name, _)| {
            [
                "0011_connector_tokens.sql",
                "0018_connector_token_invalidation.sql",
            ]
            .contains(name)
        })
        .map(|(_, sql)| *sql)
        .collect();
    connect(&config.database_url)
        .unwrap()
        .get()
        .await
        .unwrap()
        .batch_execute(&format!("DROP TABLE IF EXISTS connector_tokens; {ddl}"))
        .await
        .unwrap();
    Some((config, guard))
}

#[tokio::test(flavor = "multi_thread")]
async fn each_run_uses_its_owner_token_from_postgres() {
    let Some((config, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    // « API » : alice a connecté GitHub (store PG distinct, même clé).
    let api_store = build_connector_token_store(&config, PersistenceMode::Postgres).unwrap();
    api_store
        .store(
            "alice",
            "github",
            &TokenSet {
                access_token: "ghp-alice".into(),
                refresh_token: None,
                expires_at_ms: None,
                token_type: "Bearer".into(),
                scope: Some("repo".into()),
            },
        )
        .unwrap();

    // « Worker » : son propre store PG, mode par défaut (per_user).
    let worker_store = build_connector_token_store(&config, PersistenceMode::Postgres).unwrap();
    let resolver = ConnectorCredentialsMode::PerUser.resolver(worker_store, Arc::new(NoRefresh));
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let runs = Arc::new(InMemoryRunStateStore::new());
    for (owner, wf, run) in [("alice", "wf-a", "run-a"), ("bob", "wf-b", "run-b")] {
        repo.create(owner, github_workflow(wf)).await.unwrap();
        runs.checkpoint(
            &RunCheckpoint::queued(run, wf, 1, "manual", 0).with_owner(Some(owner.into())),
        )
        .await
        .unwrap();
    }
    let http = RecordingHttp::default();
    let exec = RunExecutor::with_http(repo, Arc::clone(&runs) as Arc<_>, resolver, http.clone());

    // Run d'alice : son token, déchiffré depuis PostgreSQL.
    assert_eq!(
        exec.handle(&delivery("run-a", "wf-a")).await,
        HandlerVerdict::Ok
    );
    {
        let sent = http.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer ghp-alice")
        );
    }

    // Run de bob : aucun token → échec explicite, aucune requête.
    assert_eq!(
        exec.handle(&delivery("run-b", "wf-b")).await,
        HandlerVerdict::DeadLetter
    );
    assert_eq!(
        http.sent.lock().unwrap().len(),
        1,
        "pas de requête pour bob"
    );
    let cp = runs.load("run-b").await.unwrap().unwrap();
    assert_eq!(cp.status, RunStatus::Failed);
    assert!(
        cp.error
            .as_deref()
            .unwrap_or("")
            .contains("propriétaire `bob`"),
        "{:?}",
        cp.error
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_token_is_refreshed_and_persisted_in_postgres() {
    let Some((config, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    // Token Gmail d'alice expiré depuis longtemps, refresh token conservé.
    let api_store = build_connector_token_store(&config, PersistenceMode::Postgres).unwrap();
    api_store
        .store(
            "alice",
            "gmail",
            &TokenSet {
                access_token: "ya29-expired".into(),
                refresh_token: Some("rt-alice".into()),
                expires_at_ms: Some(1_000),
                token_type: "Bearer".into(),
                scope: Some("gmail.send".into()),
            },
        )
        .unwrap();

    let worker_store = build_connector_token_store(&config, PersistenceMode::Postgres).unwrap();
    let resolver = ConnectorCredentialsMode::PerUser.resolver(worker_store, Arc::new(FakeGoogle));
    let mut wf = github_workflow("wf-g");
    wf.nodes[1].kind = NodeKind::Action {
        action_type: "gmail.send_email".into(),
    };
    wf.nodes[1].params = json!({ "to": "a@b.c", "subject": "s", "body": "b" });
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let runs = Arc::new(InMemoryRunStateStore::new());
    repo.create("alice", wf).await.unwrap();
    runs.checkpoint(
        &RunCheckpoint::queued("run-g", "wf-g", 1, "manual", 0).with_owner(Some("alice".into())),
    )
    .await
    .unwrap();
    let http = RecordingHttp::default();
    let exec = RunExecutor::with_http(repo, Arc::clone(&runs) as Arc<_>, resolver, http.clone());

    assert_eq!(
        exec.handle(&delivery("run-g", "wf-g")).await,
        HandlerVerdict::Ok
    );
    let auth = http.sent.lock().unwrap()[0]
        .headers
        .get("Authorization")
        .cloned();
    assert_eq!(auth.as_deref(), Some("Bearer ya29-refreshed"));

    // Persisté chiffré en base : visible d'un autre process, refresh token gardé.
    let reread = build_connector_token_store(&config, PersistenceMode::Postgres)
        .unwrap()
        .load("alice", "gmail")
        .unwrap()
        .unwrap();
    assert_eq!(reread.access_token, "ya29-refreshed");
    assert_eq!(reread.refresh_token.as_deref(), Some("rt-alice"));
    assert_eq!(reread.scope.as_deref(), Some("gmail.send"));
}

/// Provider qui **fait tourner** le refresh token : n'accepte chaque refresh
/// token qu'une fois (rejeu ⇒ `invalid_grant`), comme les providers à
/// rotation stricte. Lent, pour que les répliques se chevauchent.
struct RotatingProvider {
    calls: Mutex<Vec<String>>,
}
impl TokenRefresher for RotatingProvider {
    fn refresh(
        &self,
        _provider: &str,
        refresh_token: &str,
        now: i64,
    ) -> Result<TokenSet, RefreshError> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            if calls.iter().any(|c| c == refresh_token) {
                return Err(RefreshError::Revoked(
                    r#"oauth refresh : http 400 — {"error":"invalid_grant"}"#.into(),
                ));
            }
            calls.push(refresh_token.to_string());
            calls.len()
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        Ok(TokenSet {
            access_token: format!("at-{n}"),
            refresh_token: Some(format!("rt-{n}")),
            expires_at_ms: Some(now + 3_600_000),
            token_type: "Bearer".into(),
            scope: None,
        })
    }
}

fn expired_rotating(refresh: &str) -> TokenSet {
    TokenSet {
        access_token: "at-expired".into(),
        refresh_token: Some(refresh.into()),
        expires_at_ms: Some(1_000),
        token_type: "Bearer".into(),
        scope: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_on_separate_pools_refresh_a_rotating_token_once() {
    let Some((config, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    build_connector_token_store(&config, PersistenceMode::Postgres)
        .unwrap()
        .store("alice", "gmail", &expired_rotating("rt-0"))
        .unwrap();

    // Trois « répliques » : chacune son pool PostgreSQL et son résolveur (donc
    // son propre verrou in-process) ; seul le verrou de ligne les sérialise.
    let provider = Arc::new(RotatingProvider {
        calls: Mutex::new(Vec::new()),
    });
    let replicas: Vec<_> = (0..3)
        .map(|_| {
            ConnectorCredentialsMode::PerUser.resolver(
                build_connector_token_store(&config, PersistenceMode::Postgres).unwrap(),
                Arc::clone(&provider) as Arc<dyn TokenRefresher + Send + Sync>,
            )
        })
        .collect();
    let barrier = Arc::new(std::sync::Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|i| {
            let replica = Arc::clone(&replicas[i % 3]);
            let barrier = Arc::clone(&barrier);
            // Chemin réel : résolution depuis le thread bloquant du moteur.
            tokio::task::spawn_blocking(move || {
                barrier.wait();
                replica.token(Some("alice"), "gmail")
            })
        })
        .collect();
    let mut tokens = Vec::new();
    for h in handles {
        tokens.push(h.await.unwrap());
    }

    assert!(
        tokens.iter().all(|t| t.as_deref() == Ok("at-1")),
        "{tokens:?}"
    );
    assert_eq!(
        provider.calls.lock().unwrap().as_slice(),
        ["rt-0"],
        "un seul appel au provider, jamais de rejeu"
    );
    let stored = build_connector_token_store(&config, PersistenceMode::Postgres)
        .unwrap()
        .load("alice", "gmail")
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.refresh_token.as_deref(),
        Some("rt-1"),
        "rotation gardée"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_credential_is_marked_in_postgres_until_reconnected() {
    let Some((config, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let api_store = build_connector_token_store(&config, PersistenceMode::Postgres).unwrap();
    api_store
        .store("alice", "gmail", &expired_rotating("rt-used"))
        .unwrap();
    // Le provider a déjà vu ce refresh token : il le refuse.
    let provider = Arc::new(RotatingProvider {
        calls: Mutex::new(vec!["rt-used".into()]),
    });
    let resolver = ConnectorCredentialsMode::PerUser.resolver(
        build_connector_token_store(&config, PersistenceMode::Postgres).unwrap(),
        Arc::clone(&provider) as Arc<dyn TokenRefresher + Send + Sync>,
    );

    let r = Arc::clone(&resolver);
    let first = tokio::task::spawn_blocking(move || r.token(Some("alice"), "gmail"))
        .await
        .unwrap()
        .unwrap_err();
    assert!(first.contains("invalid_grant"), "{first}");

    let pool = connect(&config.database_url).unwrap();
    let marked = |pool: autome_persistence::PgPool| async move {
        pool.get()
            .await
            .unwrap()
            .query_one(
                "SELECT invalidated_at_ms IS NOT NULL, invalid_reason FROM connector_tokens \
                 WHERE user_id = 'alice' AND provider_id = 'gmail'",
                &[],
            )
            .await
            .unwrap()
    };
    let row = marked(pool.clone()).await;
    assert!(row.get::<_, bool>(0), "marque posée en base");
    assert!(row.get::<_, String>(1).contains("invalid_grant"));

    // Run suivant : refus sans rappeler le provider (une seule entrée).
    let r = Arc::clone(&resolver);
    let again = tokio::task::spawn_blocking(move || r.token(Some("alice"), "gmail"))
        .await
        .unwrap()
        .unwrap_err();
    assert!(again.contains("rafraîchissement précédent"), "{again}");
    assert_eq!(provider.calls.lock().unwrap().len(), 1);

    // Reconnexion via l'API : marque effacée, rafraîchissement de nouveau possible.
    api_store
        .store("alice", "gmail", &expired_rotating("rt-fresh"))
        .unwrap();
    let row = marked(pool).await;
    assert!(!row.get::<_, bool>(0), "marque effacée par la reconnexion");
    let r = Arc::clone(&resolver);
    let token = tokio::task::spawn_blocking(move || r.token(Some("alice"), "gmail"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(token, "at-2");
}

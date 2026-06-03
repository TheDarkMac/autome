//! Tests d'intégration **black-box** de l'API HTTP (F9.3).
//!
//! Contrairement aux tests inline de chaque module (qui pilotent un
//! sous-router isolé via `tower::oneshot`), cette suite monte
//! l'**application complète** — `/health` public + routes `workflows` +
//! routes `runs` + sous-arbre `/auth` — avec un **état partagé** (un seul
//! `AuthState`, un seul repo workflows, un seul `RunStateStore`, une seule
//! piste d'audit), puis la traverse par-dessus un **vrai transport réseau**
//! (`axum::serve` sur port éphémère + client `reqwest`), comme `tests/tls.rs`.
//!
//! On vérifie ainsi les invariants **transverses** invisibles à l'échelle
//! d'un module : composition des routers sans conflit de routes,
//! propagation de l'état partagé entre domaines, parcours multi-endpoints,
//! et codes HTTP / JSON réels de bout en bout.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use autome_api::auth::{protected_router, AuthState, Clock};
use autome_api::runs::{runs_router, RunsState};
use autome_api::workflows::{
    create_workflow, delete_workflow, get_workflow, list_workflows, update_workflow, WorkflowsState,
};
use autome_core::{
    AuditSink, InMemoryAuditSink, InMemoryPermissionStore, InMemoryQueue, InMemoryRevocationStore,
    InMemoryRunStateStore, InMemoryWorkflowRepository, PermissionStore, Queue, QueueRunDispatcher,
    RevocationStore, Role, RunStateStore, TokenService, WorkflowRepository,
};
use axum::routing::get;
use axum::Router;
use serde_json::Value;

const SECRET: &[u8] = b"integration-secret-must-be-32-bytes!!";
const FIXED_NOW: i64 = 1_700_000_000;

/// Horloge épinglée — rend l'émission/vérification de tokens déterministe.
struct FixedClock;
impl Clock for FixedClock {
    fn now_unix_seconds(&self) -> i64 {
        FIXED_NOW
    }
}

/// Application assemblée + tous les leviers nécessaires aux assertions
/// (stores partagés, émission/révocation de tokens).
struct TestApp {
    base: String,
    client: reqwest::Client,
    svc: TokenService,
    perms: Arc<InMemoryPermissionStore>,
    audit: Arc<InMemoryAuditSink>,
    revocation: Arc<InMemoryRevocationStore>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl TestApp {
    /// Monte l'app complète et la sert sur un port éphémère local.
    async fn spawn() -> Self {
        let revocation = Arc::new(InMemoryRevocationStore::new());
        let perms = Arc::new(InMemoryPermissionStore::new());
        let audit = Arc::new(InMemoryAuditSink::new());
        let workflows = Arc::new(InMemoryWorkflowRepository::new());
        let runs_store = Arc::new(InMemoryRunStateStore::new());
        let queue = Arc::new(InMemoryQueue::new());
        let dispatcher = Arc::new(QueueRunDispatcher::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            Arc::clone(&runs_store) as Arc<dyn RunStateStore + Send + Sync>,
        ));

        // Un *seul* AuthState partagé par les trois domaines : un token et
        // une attribution de rôle valent pour toute l'app, et l'audit
        // s'accumule dans une unique piste.
        let auth = AuthState::new(
            TokenService::new(SECRET),
            Arc::clone(&revocation) as Arc<dyn RevocationStore + Send + Sync>,
            Arc::clone(&perms) as Arc<dyn PermissionStore + Send + Sync>,
            Arc::clone(&audit) as Arc<dyn AuditSink + Send + Sync>,
        )
        .with_clock(Arc::new(FixedClock));

        let wf_state = WorkflowsState::new(
            auth.clone(),
            Arc::clone(&workflows) as Arc<dyn WorkflowRepository + Send + Sync>,
        );
        let runs_state = RunsState::new(
            auth.clone(),
            Arc::clone(&workflows) as Arc<dyn WorkflowRepository + Send + Sync>,
            Arc::clone(&runs_store) as Arc<dyn RunStateStore + Send + Sync>,
            dispatcher,
        );

        // Les routes workflows sont reconstruites à plat (`/workflows`,
        // `/workflows/:id`) plutôt que `nest`ées : le catch-all interne du
        // nest entrerait en conflit avec `/workflows/:id/runs` du router
        // runs. À plat, matchit accepte le param `:id` + son enfant statique.
        let workflows_routes = Router::new()
            .route("/workflows", get(list_workflows).post(create_workflow))
            .route(
                "/workflows/:id",
                get(get_workflow)
                    .put(update_workflow)
                    .delete(delete_workflow),
            )
            .with_state(wf_state);

        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .merge(workflows_routes)
            .merge(runs_router(runs_state))
            .nest("/auth", protected_router(auth));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        Self {
            base: format!("http://{addr}"),
            client,
            svc: TokenService::new(SECRET),
            perms,
            audit,
            revocation,
            server,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Attribue `role` à `user` et émet un access token valide.
    fn token(&self, user: &str, role: Role) -> String {
        self.perms.assign_role(user, &role).unwrap();
        self.svc.issue_access(user, 900, FIXED_NOW).unwrap()
    }

    /// Émet un token pour un utilisateur **sans aucun rôle** (→ 403 partout).
    fn token_no_role(&self, user: &str) -> String {
        self.svc.issue_access(user, 900, FIXED_NOW).unwrap()
    }

    /// Révoque un token via son `jti` (extrait par vérification).
    async fn revoke(&self, token: &str) {
        let claims = self.svc.verify(token, FIXED_NOW).unwrap();
        self.revocation
            .revoke(&claims.jti, claims.exp.saturating_mul(1_000))
            .await
            .unwrap();
    }
}

/// Payload JSON d'un workflow minimal (un seul trigger manuel).
fn workflow_payload(id: &str) -> String {
    serde_json::json!({
        "id": id,
        "name": format!("workflow {id}"),
        "nodes": [
            { "id": "entry", "name": "entry", "kind": "trigger", "trigger_type": "manual" }
        ],
        "edges": []
    })
    .to_string()
}

// ───────────────────────── F9.3.c — surface publique / auth ─────────────

#[tokio::test]
async fn health_is_public_without_token() {
    let app = TestApp::spawn().await;
    let resp = app.client.get(app.url("/health")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn shared_auth_state_authorizes_whoami() {
    // Un token émis par le service partagé est accepté sur le sous-arbre
    // `/auth` monté avec le *même* AuthState que workflows/runs.
    let app = TestApp::spawn().await;
    let token = app.token_no_role("alice");
    let resp = app
        .client
        .get(app.url("/auth/whoami"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn protected_routes_return_401_without_authorization() {
    // Invariant transverse : toute route protégée exige un Bearer, quel
    // que soit le domaine (workflows / runs / auth).
    let app = TestApp::spawn().await;
    let cases: &[(&str, &str)] = &[
        ("GET", "/workflows"),
        ("POST", "/workflows"),
        ("GET", "/workflows/wf-1"),
        ("POST", "/workflows/wf-1/runs"),
        ("GET", "/workflows/wf-1/runs"),
        ("GET", "/runs/run-1"),
        ("GET", "/auth/whoami"),
    ];
    for (method, path) in cases {
        let req = match *method {
            "GET" => app.client.get(app.url(path)),
            "POST" => app.client.post(app.url(path)),
            _ => unreachable!(),
        };
        let resp = req.send().await.unwrap();
        assert_eq!(
            resp.status(),
            401,
            "{method} {path} doit exiger une authentification"
        );
    }
}

#[tokio::test]
async fn viewer_can_read_but_not_write() {
    // Le rôle viewer (workflows.read + runs.read) lit partout mais ne peut
    // ni créer un workflow ni déclencher un run.
    let app = TestApp::spawn().await;
    let token = app.token("vicky", Role::viewer());

    let read = app
        .client
        .get(app.url("/workflows"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        read.status(),
        200,
        "lecture workflows autorisée pour viewer"
    );

    let create = app
        .client
        .post(app.url("/workflows"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(workflow_payload("wf-deny"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        create.status(),
        403,
        "écriture workflow interdite au viewer"
    );

    // La vérification de permission précède le contrôle d'existence du
    // workflow : un viewer reçoit 403 même sur un workflow inexistant.
    let run = app
        .client
        .post(app.url("/workflows/wf-deny/runs"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(run.status(), 403, "exécution de run interdite au viewer");
}

#[tokio::test]
async fn user_without_role_gets_403_on_read() {
    let app = TestApp::spawn().await;
    let token = app.token_no_role("nobody");
    let resp = app
        .client
        .get(app.url("/workflows"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn revoked_token_is_rejected_across_the_whole_app() {
    // La révocation porte sur le `jti` : une fois révoqué, le token est
    // refusé sur *toutes* les routes de l'app (état d'auth partagé).
    let app = TestApp::spawn().await;
    let token = app.token("eve", Role::editor());

    let before = app
        .client
        .get(app.url("/workflows"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(before.status(), 200, "token valide avant révocation");

    app.revoke(&token).await;

    for path in ["/workflows", "/auth/whoami"] {
        let resp = app
            .client
            .get(app.url(path))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{path} doit refuser un token révoqué");
    }
}

// ───────────────────────── F9.3.b — parcours de cycle de vie ────────────

#[tokio::test]
async fn full_lifecycle_create_workflow_then_run_then_read() {
    let app = TestApp::spawn().await;
    let token = app.token("alice", Role::editor());

    // 1. Création du workflow → 201.
    let create = app
        .client
        .post(app.url("/workflows"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(workflow_payload("wf-life"))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 201);

    // 2. Relecture → 200, id cohérent.
    let got = app
        .client
        .get(app.url("/workflows/wf-life"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let wf: Value = serde_json::from_str(&got.text().await.unwrap()).unwrap();
    assert_eq!(wf["id"], "wf-life");

    // 3. Déclenchement d'un run → 202 + run_id + status "queued".
    let triggered = app
        .client
        .post(app.url("/workflows/wf-life/runs"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(serde_json::json!({ "payload": { "hello": "world" } }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(triggered.status(), 202);
    let created: Value = serde_json::from_str(&triggered.text().await.unwrap()).unwrap();
    assert_eq!(created["status"], "queued");
    let run_id = created["run_id"]
        .as_str()
        .expect("run_id présent")
        .to_string();

    // 4. Lecture du run via le store partagé → 200, état Queued, bon workflow.
    let run = app
        .client
        .get(app.url(&format!("/runs/{run_id}")))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(run.status(), 200);
    let snap: Value = serde_json::from_str(&run.text().await.unwrap()).unwrap();
    assert_eq!(snap["run_id"], run_id);
    assert_eq!(snap["workflow_id"], "wf-life");
    assert_eq!(snap["status"], "Queued");

    // 5. Le run apparaît dans la liste du workflow.
    let list = app
        .client
        .get(app.url("/workflows/wf-life/runs"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let page: Value = serde_json::from_str(&list.text().await.unwrap()).unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["run_id"], run_id);
}

// ───────────────────────── F9.3.d — cohérence inter-domaines ────────────

#[tokio::test]
async fn run_on_unknown_workflow_returns_404() {
    // Le domaine runs lit le *même* repo que workflows : déclencher un run
    // sur un workflow absent renvoie 404 (et non 202).
    let app = TestApp::spawn().await;
    let token = app.token("alice", Role::editor());
    let resp = app
        .client
        .post(app.url("/workflows/ghost/runs"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn audit_trail_spans_workflows_and_runs_for_same_actor() {
    // Après un parcours create-workflow + create-run, la piste d'audit
    // *partagée* contient les deux familles d'events pour le même acteur.
    let app = TestApp::spawn().await;
    let token = app.token("alice", Role::editor());

    app.client
        .post(app.url("/workflows"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(workflow_payload("wf-audit"))
        .send()
        .await
        .unwrap();
    app.client
        .post(app.url("/workflows/wf-audit/runs"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();

    let events = app.audit.list().await.unwrap();

    let wf_create = events
        .iter()
        .find(|e| e.action == "workflows.create")
        .expect("event workflows.create présent");
    assert_eq!(wf_create.actor, "alice");
    assert_eq!(wf_create.resource.as_deref(), Some("wf-audit"));

    let run_create = events
        .iter()
        .find(|e| e.action == "runs.create")
        .expect("event runs.create présent");
    assert_eq!(run_create.actor, "alice");

    // Même acteur de bout en bout sur les deux domaines.
    assert_eq!(wf_create.actor, run_create.actor);
}

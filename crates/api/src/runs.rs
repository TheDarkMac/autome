//! Déclenchement manuel & suivi d'un run (F7.2.b/c/d/e).
//!
//! Endpoints REST/JSON :
//!
//! | Méthode | Path                       | Permission       | Description                          |
//! |---------|----------------------------|------------------|--------------------------------------|
//! | POST    | `/workflows/{id}/runs`     | `runs.execute`   | Démarre un run (202 + `run_id`)      |
//! | GET     | `/workflows/{id}/runs`     | `runs.read`      | Liste paginée des runs du workflow   |
//! | GET     | `/runs/{run_id}`           | `runs.read`      | Détail (404 si inconnu)              |
//!
//! Toutes les routes sont protégées par [`crate::auth::AuthenticatedUser`]
//! + check `require_permission`. Chaque appel enregistre un event audit
//!   `runs.<verb>` (actor = user_id, resource = run_id / workflow_id).
//!
//! **Isolation par propriétaire** (backlog BOLA) : un run appartient au
//! propriétaire de son workflow au moment du dispatch (`RunCheckpoint::owner_id`).
//! Les routes `/workflows/{id}/runs` opèrent dans l'espace d'ids de workflow
//! de l'appelant (#188, cf. [`crate::workflows`]) : démarrer un run vise son
//! propre workflow `id`, la liste ne contient que ses runs. Lire le run
//! d'autrui répond 404. Un super-admin (`*`) vise l'espace d'un autre avec
//! `?owner=<user_id>` et lit tout run par son id.

use std::sync::Arc;

use autome_core::{
    AuditEvent, AuditOutcome, DispatchRequest, Pagination, RunCheckpoint, RunDispatcher,
    RunListPage, RunStateStore, WorkflowId, WorkflowRepository, PERM_RUNS_EXECUTE, PERM_RUNS_READ,
};
use axum::{
    extract::{FromRef, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::auth::{AuthError, AuthState, AuthenticatedUser};
use crate::workflows::NamespaceQuery;

/// État partagé par les handlers runs : auth + dispatcher + stores.
#[derive(Clone)]
pub struct RunsState {
    /// État d'authentification (token, perms, clock, audit).
    pub auth: AuthState,
    /// Repository des workflows (lecture seule : on vérifie l'existence
    /// avant de dispatcher un run).
    pub workflows: Arc<dyn WorkflowRepository + Send + Sync>,
    /// Store d'état des runs (lecture pour GET, le dispatcher écrit en
    /// interne).
    pub runs: Arc<dyn RunStateStore + Send + Sync>,
    /// Déclencheur de runs (persistance + publication queue).
    pub dispatcher: Arc<dyn RunDispatcher + Send + Sync>,
}

impl RunsState {
    /// Builder convenience.
    pub fn new(
        auth: AuthState,
        workflows: Arc<dyn WorkflowRepository + Send + Sync>,
        runs: Arc<dyn RunStateStore + Send + Sync>,
        dispatcher: Arc<dyn RunDispatcher + Send + Sync>,
    ) -> Self {
        Self {
            auth,
            workflows,
            runs,
            dispatcher,
        }
    }
}

// Permet à `AuthenticatedUser` d'être extrait dans un router parameteré
// par `RunsState`.
impl FromRef<RunsState> for AuthState {
    fn from_ref(input: &RunsState) -> Self {
        input.auth.clone()
    }
}

/// Body JSON du POST. `payload` est libre (transmis tel quel au worker).
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct CreateRunBody {
    /// Payload initial du run (entrées du workflow). `null` si absent.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub payload: Value,
    /// Version du workflow à figer. Si absent, on prend `1` (la base
    /// PostgreSQL gérera l'auto-incrément en F1.5/F7.x).
    #[serde(default)]
    pub version: Option<i32>,
}

/// Réponse du POST.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreateRunResponse {
    /// Identifiant du run créé (string opaque côté API).
    pub run_id: String,
    /// État initial (`"queued"`).
    pub status: String,
}

/// Paramètres de query de la liste (`?offset=0&limit=50&owner=`).
#[derive(Debug, Deserialize, IntoParams)]
pub struct ListQuery {
    /// Offset, défaut 0.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Limite, défaut 50.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Espace d'ids de workflow d'un autre utilisateur (super-admin
    /// seulement) — cf. [`NamespaceQuery`].
    #[serde(default)]
    pub owner: Option<String>,
}

/// Réponse JSON de la liste paginée.
#[derive(Debug, Serialize, ToSchema)]
pub struct ListRunsResponse {
    /// Items de la page (plus récent en premier — schéma libre côté OpenAPI).
    #[schema(value_type = Vec<Object>)]
    pub items: Vec<RunCheckpoint>,
    /// Total avant pagination.
    pub total: usize,
    /// Offset effectivement appliqué.
    pub offset: usize,
    /// Limit effectivement appliquée.
    pub limit: usize,
}

/// Erreur métier des handlers runs. Mappée vers les codes HTTP standards.
#[derive(Debug)]
pub enum RunError {
    /// Erreur d'auth/RBAC (propagation depuis le middleware).
    Auth(AuthError),
    /// Run ou workflow non trouvé.
    NotFound(String),
    /// Erreur backend (dispatcher, store).
    Internal(String),
}

impl From<AuthError> for RunError {
    fn from(e: AuthError) -> Self {
        Self::Auth(e)
    }
}

impl IntoResponse for RunError {
    fn into_response(self) -> Response {
        match self {
            RunError::Auth(e) => e.into_response(),
            RunError::NotFound(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
            RunError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        }
    }
}

async fn audit_event(
    state: &RunsState,
    actor: &str,
    action: &str,
    resource: &str,
    outcome: AuditOutcome,
) {
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let event = AuditEvent::new(now_ms, actor, action, outcome).with_resource(resource);
    if let Err(e) = state.auth.audit.record(event).await {
        tracing::error!(action, error = %e, "événement d'audit non enregistré");
    }
}

/// `POST /workflows/{id}/runs` — démarre un run (perm `runs.execute`).
#[utoipa::path(
    post,
    path = "/workflows/{id}/runs",
    tag = "runs",
    params(
        ("id" = String, Path, description = "Identifiant du workflow à exécuter"),
        NamespaceQuery
    ),
    request_body(content = CreateRunBody, description = "Payload initial + version optionnelle"),
    responses(
        (status = 202, description = "Run accepté en queue", body = CreateRunResponse),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `runs.execute`, ou `owner` d'autrui sans `*`"),
        (status = 404, description = "Workflow inconnu dans cet espace")
    ),
    security(("bearer" = []))
)]
pub async fn create_run(
    user: AuthenticatedUser,
    State(state): State<RunsState>,
    Path(workflow_id): Path<String>,
    Query(q): Query<NamespaceQuery>,
    body: Option<Json<CreateRunBody>>,
) -> Result<(StatusCode, Json<CreateRunResponse>), RunError> {
    state
        .auth
        .require_permission(&user, PERM_RUNS_EXECUTE)
        .await?;

    // 1. Le workflow doit exister dans l'espace visé (celui de l'appelant,
    //    sauf `?owner=` d'un super-admin) — sinon 404.
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let exists = state
        .workflows
        .get(&ns, &WorkflowId::new(&workflow_id))
        .await
        .map_err(|e| RunError::Internal(e.to_string()))?
        .is_some();
    if !exists {
        audit_event(
            &state,
            user.user_id(),
            "runs.create",
            &workflow_id,
            AuditOutcome::Failure,
        )
        .await;
        return Err(RunError::NotFound(format!(
            "workflow '{workflow_id}' inconnu"
        )));
    }

    // 2. Dispatch.
    let Json(body) = body.unwrap_or_default();
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let req = DispatchRequest {
        workflow_id: workflow_id.clone(),
        version: body.version.unwrap_or(1),
        trigger_type: "manual".into(),
        payload: body.payload,
        now_ms,
        // Le run appartient au propriétaire du workflow (l'espace visé), même
        // s'il est déclenché par un super-admin ; le worker relit le workflow
        // dans cet espace.
        owner_id: Some(ns),
    };
    let outcome = state
        .dispatcher
        .dispatch(req)
        .await
        .map_err(|e| RunError::Internal(e.to_string()))?;

    audit_event(
        &state,
        user.user_id(),
        "runs.create",
        &outcome.run_id,
        AuditOutcome::Success,
    )
    .await;

    Ok((
        StatusCode::ACCEPTED,
        Json(CreateRunResponse {
            run_id: outcome.run_id,
            status: "queued".into(),
        }),
    ))
}

/// `GET /runs/{run_id}` — détail d'un run (perm `runs.read`).
#[utoipa::path(
    get,
    path = "/runs/{run_id}",
    tag = "runs",
    params(("run_id" = String, Path, description = "Identifiant du run")),
    responses(
        (status = 200, description = "Détail du run", body = serde_json::Value),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `runs.read`"),
        (status = 404, description = "Run inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn get_run(
    user: AuthenticatedUser,
    State(state): State<RunsState>,
    Path(run_id): Path<String>,
) -> Result<Json<RunCheckpoint>, RunError> {
    state.auth.require_permission(&user, PERM_RUNS_READ).await?;
    let scope = state.auth.owner_scope(&user).await?;
    match state
        .runs
        .load(&run_id)
        .await
        .map_err(|e| RunError::Internal(e.to_string()))?
    {
        // Run d'autrui : même réponse qu'un run inconnu.
        Some(snap) if !scope.allows(snap.owner_id.as_deref()) => {
            audit_event(
                &state,
                user.user_id(),
                "runs.get",
                &run_id,
                AuditOutcome::Denied,
            )
            .await;
            Err(RunError::NotFound(format!("run '{run_id}' inconnu")))
        }
        Some(snap) => {
            audit_event(
                &state,
                user.user_id(),
                "runs.get",
                &run_id,
                AuditOutcome::Success,
            )
            .await;
            Ok(Json(snap))
        }
        None => {
            audit_event(
                &state,
                user.user_id(),
                "runs.get",
                &run_id,
                AuditOutcome::Failure,
            )
            .await;
            Err(RunError::NotFound(format!("run '{run_id}' inconnu")))
        }
    }
}

/// `GET /workflows/{id}/runs?offset=&limit=` — liste paginée (perm `runs.read`).
#[utoipa::path(
    get,
    path = "/workflows/{id}/runs",
    tag = "runs",
    params(
        ("id" = String, Path, description = "Identifiant du workflow"),
        ListQuery
    ),
    responses(
        (status = 200, description = "Page de runs (plus récent en premier)", body = ListRunsResponse),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `runs.read`, ou `owner` d'autrui sans `*`")
    ),
    security(("bearer" = []))
)]
pub async fn list_runs(
    user: AuthenticatedUser,
    State(state): State<RunsState>,
    Path(workflow_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListRunsResponse>, RunError> {
    state.auth.require_permission(&user, PERM_RUNS_READ).await?;
    let paging = Pagination {
        offset: q.offset.unwrap_or(0),
        limit: q.limit.unwrap_or(50),
    };
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let RunListPage { items, total } = state
        .runs
        .list_by_workflow(&workflow_id, Some(&ns), paging)
        .await
        .map_err(|e| RunError::Internal(e.to_string()))?;
    audit_event(
        &state,
        user.user_id(),
        "runs.list",
        &workflow_id,
        AuditOutcome::Success,
    )
    .await;
    Ok(Json(ListRunsResponse {
        items,
        total,
        offset: paging.offset,
        limit: paging.limit,
    }))
}

/// Construit le sous-router runs : 3 routes (`POST /workflows/:id/runs`,
/// `GET /workflows/:id/runs`, `GET /runs/:run_id`). À composer avec
/// `Router::merge` au niveau racine (cf. `main.rs`).
pub fn runs_router(state: RunsState) -> Router {
    Router::new()
        .route("/workflows/:id/runs", post(create_run).get(list_runs))
        .route("/runs/:run_id", get(get_run))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{
        AuditSink, InMemoryAuditSink, InMemoryPermissionStore, InMemoryQueue,
        InMemoryRevocationStore, InMemoryRunStateStore, InMemoryWorkflowRepository, NodeId,
        NodeKind, PermissionStore, QueueRunDispatcher, RevocationStore, Role, TokenService,
        WorkflowGraph,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const FIXED_NOW: i64 = 1_700_000_000;

    struct FixedClock;
    impl crate::auth::Clock for FixedClock {
        fn now_unix_seconds(&self) -> i64 {
            FIXED_NOW
        }
    }

    fn make_test_state() -> (
        RunsState,
        TokenService,
        Arc<InMemoryPermissionStore>,
        Arc<InMemoryAuditSink>,
        Arc<InMemoryWorkflowRepository>,
        Arc<InMemoryRunStateStore>,
    ) {
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let perms = Arc::new(InMemoryPermissionStore::new());
        let audit = Arc::new(InMemoryAuditSink::new());
        let workflows = Arc::new(InMemoryWorkflowRepository::new());
        let runs_store = Arc::new(InMemoryRunStateStore::new());
        let queue = Arc::new(InMemoryQueue::new());
        let dispatcher = Arc::new(
            QueueRunDispatcher::new(
                Arc::clone(&queue) as Arc<dyn autome_core::Queue + Send + Sync>,
                Arc::clone(&runs_store) as Arc<dyn RunStateStore + Send + Sync>,
            )
            .with_id_generator(|| "fixed-run".to_string()),
        );
        let svc = TokenService::new(SECRET);
        let auth = AuthState::new(
            TokenService::new(SECRET),
            revocation,
            Arc::clone(&perms) as Arc<dyn PermissionStore + Send + Sync>,
            Arc::clone(&audit) as Arc<dyn AuditSink + Send + Sync>,
        )
        .with_clock(Arc::new(FixedClock));
        let state = RunsState::new(
            auth,
            Arc::clone(&workflows) as Arc<dyn WorkflowRepository + Send + Sync>,
            Arc::clone(&runs_store) as Arc<dyn RunStateStore + Send + Sync>,
            dispatcher,
        );
        (state, svc, perms, audit, workflows, runs_store)
    }

    fn sample_workflow(id: &str) -> WorkflowGraph {
        WorkflowGraph {
            id: WorkflowId::new(id),
            name: format!("workflow {id}"),
            nodes: vec![autome_core::Node {
                id: NodeId::new("entry"),
                name: "entry".into(),
                kind: NodeKind::Trigger {
                    trigger_type: "manual".into(),
                },
                params: serde_json::Value::Null,
            }],
            edges: vec![],
        }
    }

    async fn body_string(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn auth_req(method: &str, uri: &str, token: &str, body: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"));
        if body.is_some() {
            b = b.header("Content-Type", "application/json");
        }
        b.body(
            body.map(|s| Body::from(s.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap()
    }

    // ───── F7.2.b — POST /workflows/:id/runs ─────

    #[tokio::test]
    async fn create_run_returns_202_and_run_id_when_workflow_exists() {
        let (state, svc, perms, _, workflows, runs_store) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req("POST", "/workflows/wf-1/runs", &token, Some("{}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["run_id"], "fixed-run");
        assert_eq!(body["status"], "queued");
        // Le checkpoint a bien été créé en `Queued`.
        let snap = runs_store.load("fixed-run").await.unwrap().unwrap();
        assert_eq!(snap.workflow_id, "wf-1");
        assert_eq!(snap.status, autome_core::RunStatus::Queued);
        assert_eq!(snap.trigger_type, "manual");
    }

    #[tokio::test]
    async fn create_run_with_no_body_uses_defaults() {
        let (state, svc, perms, _, workflows, _) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req("POST", "/workflows/wf-1/runs", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn create_run_returns_404_when_workflow_unknown() {
        let (state, svc, perms, _, _, _) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req(
                "POST",
                "/workflows/missing/runs",
                &token,
                Some("{}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_run_passes_payload_to_dispatcher() {
        let (state, svc, perms, _, workflows, runs_store) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let _ = app
            .oneshot(auth_req(
                "POST",
                "/workflows/wf-1/runs",
                &token,
                Some(r#"{"version": 7, "payload": {"k": "v"}}"#),
            ))
            .await
            .unwrap();
        let snap = runs_store.load("fixed-run").await.unwrap().unwrap();
        assert_eq!(snap.version, 7);
        // created_at_ms = FIXED_NOW * 1000
        assert_eq!(snap.created_at_ms, FIXED_NOW * 1000);
    }

    // ───── F7.2.c — GET /runs/:run_id ─────

    #[tokio::test]
    async fn get_run_returns_200_when_exists() {
        let (state, svc, perms, _, _, runs_store) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let snap = autome_core::RunCheckpoint::queued("r-1", "wf-1", 1, "manual", 1000)
            .with_owner(Some("alice".into()));
        runs_store.checkpoint(&snap).await.unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req("GET", "/runs/r-1", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["run_id"], "r-1");
        assert_eq!(body["workflow_id"], "wf-1");
    }

    #[tokio::test]
    async fn get_run_returns_404_when_unknown() {
        let (state, svc, perms, _, _, _) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req("GET", "/runs/missing", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ───── F7.2.d — GET /workflows/:id/runs ─────

    #[tokio::test]
    async fn list_runs_returns_runs_for_workflow_in_recent_first_order() {
        let (state, svc, perms, _, _, runs_store) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        for (id, created) in [("old", 100), ("new", 300), ("mid", 200)] {
            let snap = autome_core::RunCheckpoint::queued(id, "wf-1", 1, "manual", created)
                .with_owner(Some("alice".into()));
            runs_store.checkpoint(&snap).await.unwrap();
        }
        // Run d'un autre workflow, doit être filtré.
        let other = autome_core::RunCheckpoint::queued("other", "wf-2", 1, "manual", 500)
            .with_owner(Some("alice".into()));
        runs_store.checkpoint(&other).await.unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req("GET", "/workflows/wf-1/runs", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["total"], 3);
        let ids: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["run_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["new", "mid", "old"]);
    }

    #[tokio::test]
    async fn list_runs_pagination_query_params_work() {
        let (state, svc, perms, _, _, runs_store) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        for i in 0..5 {
            let snap = autome_core::RunCheckpoint::queued(
                format!("r{i}"),
                "wf-1",
                1,
                "manual",
                1000 - i as i64,
            )
            .with_owner(Some("alice".into()));
            runs_store.checkpoint(&snap).await.unwrap();
        }
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req(
                "GET",
                "/workflows/wf-1/runs?offset=1&limit=2",
                &token,
                None,
            ))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["total"], 5);
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        assert_eq!(body["offset"], 1);
        assert_eq!(body["limit"], 2);
    }

    // ───── F7.2.e — RBAC ─────

    #[tokio::test]
    async fn create_run_returns_401_without_auth() {
        let (state, _, _, _, _, _) = make_test_state();
        let app = runs_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/workflows/wf-1/runs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn create_run_returns_403_for_viewer() {
        // Viewer n'a pas runs.execute → 403 (workflows.read seul ne suffit pas).
        let (state, svc, perms, _, workflows, _) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req("POST", "/workflows/wf-1/runs", &token, Some("{}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_run_returns_403_without_read_perm() {
        // Pas de rôle → 403.
        let (state, svc, _, _, _, _) = make_test_state();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let response = app
            .oneshot(auth_req("GET", "/runs/r-1", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ───── F7.2.e — Audit ─────

    #[tokio::test]
    async fn create_run_records_audit_event_with_run_id() {
        let (state, svc, perms, audit, workflows, _) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let _ = app
            .oneshot(auth_req("POST", "/workflows/wf-1/runs", &token, Some("{}")))
            .await
            .unwrap();
        let events = audit.list().await.unwrap();
        let create_evt = events
            .iter()
            .find(|e| e.action == "runs.create" && e.outcome == AuditOutcome::Success)
            .expect("runs.create success event présent");
        assert_eq!(create_evt.actor, "alice");
        assert_eq!(create_evt.resource.as_deref(), Some("fixed-run"));
    }

    #[tokio::test]
    async fn create_run_records_failure_audit_event_when_workflow_unknown() {
        let (state, svc, perms, audit, _, _) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let _ = app
            .oneshot(auth_req(
                "POST",
                "/workflows/missing/runs",
                &token,
                Some("{}"),
            ))
            .await
            .unwrap();
        let events = audit.list().await.unwrap();
        let evt = events
            .iter()
            .find(|e| e.action == "runs.create" && e.outcome == AuditOutcome::Failure)
            .expect("runs.create failure event présent");
        assert_eq!(evt.resource.as_deref(), Some("missing"));
    }

    #[tokio::test]
    async fn list_runs_records_audit_event_with_workflow_id() {
        let (state, svc, perms, audit, _, _) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        let _ = app
            .oneshot(auth_req("GET", "/workflows/wf-1/runs", &token, None))
            .await
            .unwrap();
        let events = audit.list().await.unwrap();
        let evt = events
            .iter()
            .find(|e| e.action == "runs.list")
            .expect("runs.list event présent");
        assert_eq!(evt.resource.as_deref(), Some("wf-1"));
    }

    // ───── Backlog BOLA — isolation par propriétaire ─────

    #[tokio::test]
    async fn run_inherits_the_workflow_owner() {
        let (state, svc, perms, _, workflows, runs_store) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let response = runs_router(state)
            .oneshot(auth_req("POST", "/workflows/wf-1/runs", &token, Some("{}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let snap = runs_store.load("fixed-run").await.unwrap().unwrap();
        assert_eq!(snap.owner_id.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn cannot_start_a_run_on_someone_elses_workflow() {
        let (state, svc, perms, audit, workflows, runs_store) = make_test_state();
        perms.assign_role("bob", &Role::editor()).unwrap();
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let token = svc.issue_access("bob", 900, FIXED_NOW).unwrap();
        let response = runs_router(state)
            .oneshot(auth_req("POST", "/workflows/wf-1/runs", &token, Some("{}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(runs_store.load("fixed-run").await.unwrap().is_none());
        let events = audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "runs.create" && e.outcome == AuditOutcome::Failure));
    }

    #[tokio::test]
    async fn cannot_read_someone_elses_run() {
        let (state, svc, perms, audit, _, runs_store) = make_test_state();
        perms.assign_role("bob", &Role::viewer()).unwrap();
        let snap = autome_core::RunCheckpoint::queued("r-1", "wf-1", 1, "manual", 1000)
            .with_owner(Some("alice".into()));
        runs_store.checkpoint(&snap).await.unwrap();
        let token = svc.issue_access("bob", 900, FIXED_NOW).unwrap();
        let response = runs_router(state)
            .oneshot(auth_req("GET", "/runs/r-1", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!body_string(response).await.contains("alice"));
        let events = audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "runs.get" && e.outcome == AuditOutcome::Denied));
    }

    #[tokio::test]
    async fn run_without_owner_is_only_visible_to_super_admin() {
        let (state, svc, perms, _, _, runs_store) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        perms.assign_role("root", &Role::super_admin()).unwrap();
        let legacy = autome_core::RunCheckpoint::queued("r-legacy", "wf-1", 1, "manual", 1);
        runs_store.checkpoint(&legacy).await.unwrap();
        let app = runs_router(state);
        let alice = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let root = svc.issue_access("root", 900, FIXED_NOW).unwrap();
        let r = app
            .clone()
            .oneshot(auth_req("GET", "/runs/r-legacy", &alice, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = app
            .oneshot(auth_req("GET", "/runs/r-legacy", &root, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn listing_shows_only_the_callers_runs() {
        let (state, svc, perms, _, _, runs_store) = make_test_state();
        perms.assign_role("bob", &Role::viewer()).unwrap();
        perms.assign_role("root", &Role::super_admin()).unwrap();
        for (id, owner) in [("a1", "alice"), ("a2", "alice"), ("b1", "bob")] {
            let snap = autome_core::RunCheckpoint::queued(id, "wf-1", 1, "manual", 1)
                .with_owner(Some(owner.into()));
            runs_store.checkpoint(&snap).await.unwrap();
        }
        let app = runs_router(state);
        let bob = svc.issue_access("bob", 900, FIXED_NOW).unwrap();
        let body: serde_json::Value = serde_json::from_str(
            &body_string(
                app.clone()
                    .oneshot(auth_req("GET", "/workflows/wf-1/runs", &bob, None))
                    .await
                    .unwrap(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(body["total"], 1);
        assert_eq!(body["items"][0]["run_id"], "b1");

        // Super-admin : son propre espace par défaut, celui d'un autre avec
        // `?owner=` (#188).
        let root = svc.issue_access("root", 900, FIXED_NOW).unwrap();
        let total = |uri: &'static str| {
            let (app, root) = (app.clone(), root.clone());
            async move {
                let body: serde_json::Value = serde_json::from_str(
                    &body_string(
                        app.oneshot(auth_req("GET", uri, &root, None))
                            .await
                            .unwrap(),
                    )
                    .await,
                )
                .unwrap();
                body["total"].clone()
            }
        };
        assert_eq!(total("/workflows/wf-1/runs").await, 0);
        assert_eq!(total("/workflows/wf-1/runs?owner=alice").await, 2);
        // Un utilisateur ordinaire ne vise pas l'espace d'autrui.
        let r = app
            .oneshot(auth_req(
                "GET",
                "/workflows/wf-1/runs?owner=alice",
                &bob,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn recreated_workflow_does_not_expose_previous_owners_runs() {
        let (state, svc, perms, _, workflows, runs_store) = make_test_state();
        perms.assign_role("bob", &Role::editor()).unwrap();
        // Run d'alice sur `wf-1`, puis `wf-1` supprimé et recréé par bob.
        let old = autome_core::RunCheckpoint::queued("r-alice", "wf-1", 1, "manual", 1)
            .with_owner(Some("alice".into()));
        runs_store.checkpoint(&old).await.unwrap();
        workflows
            .create("bob", sample_workflow("wf-1"))
            .await
            .unwrap();
        let app = runs_router(state);
        let bob = svc.issue_access("bob", 900, FIXED_NOW).unwrap();
        let list: serde_json::Value = serde_json::from_str(
            &body_string(
                app.clone()
                    .oneshot(auth_req("GET", "/workflows/wf-1/runs", &bob, None))
                    .await
                    .unwrap(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(list["total"], 0);
        let r = app
            .oneshot(auth_req("GET", "/runs/r-alice", &bob, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn super_admin_run_on_users_workflow_belongs_to_the_owner() {
        let (state, svc, perms, _, workflows, runs_store) = make_test_state();
        perms.assign_role("root", &Role::super_admin()).unwrap();
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let root = svc.issue_access("root", 900, FIXED_NOW).unwrap();
        let app = runs_router(state);
        // Sans `owner`, root vise son propre espace, où `wf-1` n'existe pas.
        let r = app
            .clone()
            .oneshot(auth_req("POST", "/workflows/wf-1/runs", &root, Some("{}")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = app
            .oneshot(auth_req(
                "POST",
                "/workflows/wf-1/runs?owner=alice",
                &root,
                Some("{}"),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let snap = runs_store.load("fixed-run").await.unwrap().unwrap();
        assert_eq!(snap.owner_id.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn same_workflow_id_runs_in_each_owners_namespace() {
        let (state, svc, perms, _, workflows, runs_store) = make_test_state();
        perms.assign_role("bob", &Role::editor()).unwrap();
        // `wf-1` existe chez alice ; bob n'en a pas encore.
        workflows
            .create("alice", sample_workflow("wf-1"))
            .await
            .unwrap();
        let app = runs_router(state);
        let bob = svc.issue_access("bob", 900, FIXED_NOW).unwrap();
        let post = || {
            let (app, bob) = (app.clone(), bob.clone());
            async move {
                app.oneshot(auth_req("POST", "/workflows/wf-1/runs", &bob, Some("{}")))
                    .await
                    .unwrap()
                    .status()
            }
        };
        assert_eq!(post().await, StatusCode::NOT_FOUND, "le wf-1 d'alice");
        workflows
            .create("bob", sample_workflow("wf-1"))
            .await
            .unwrap();
        assert_eq!(post().await, StatusCode::ACCEPTED);
        let snap = runs_store.load("fixed-run").await.unwrap().unwrap();
        assert_eq!(snap.owner_id.as_deref(), Some("bob"));
    }
}

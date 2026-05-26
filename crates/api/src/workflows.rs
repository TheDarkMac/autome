//! CRUD HTTP des workflows (F7.1.b + F7.1.c).
//!
//! Endpoints REST/JSON :
//!
//! | Méthode | Path                  | Permission         | Description                       |
//! |---------|-----------------------|--------------------|-----------------------------------|
//! | GET     | `/workflows`          | `workflows.read`   | Liste paginée                     |
//! | POST    | `/workflows`          | `workflows.write`  | Création (201 + Location)         |
//! | GET     | `/workflows/{id}`     | `workflows.read`   | Détail (404 si inconnu)           |
//! | PUT     | `/workflows/{id}`     | `workflows.write`  | Remplacement complet              |
//! | DELETE  | `/workflows/{id}`     | `workflows.write`  | Suppression (204, idempotent)     |
//!
//! Toutes les routes sont protégées par [`crate::auth::AuthenticatedUser`].
//! Chaque appel enregistre un event audit `workflows.<verb>` via
//! [`autome_core::AuditSink`] avec actor + resource = workflow_id.

use std::sync::Arc;

use autome_core::{
    AuditEvent, AuditOutcome, Pagination, WorkflowGraph, WorkflowId, WorkflowPage,
    WorkflowRepository, PERM_WORKFLOWS_READ, PERM_WORKFLOWS_WRITE,
};
use axum::{
    extract::{FromRef, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::auth::{AuthError, AuthState, AuthenticatedUser};

/// État partagé par les handlers workflows : auth + repo + audit.
#[derive(Clone)]
pub struct WorkflowsState {
    /// État d'authentification (token, perms, clock, audit).
    pub auth: AuthState,
    /// Repository des workflows.
    pub repo: Arc<dyn WorkflowRepository + Send + Sync>,
}

impl WorkflowsState {
    /// Builder convenience.
    pub fn new(auth: AuthState, repo: Arc<dyn WorkflowRepository + Send + Sync>) -> Self {
        Self { auth, repo }
    }
}

// Permet à l'extractor [`AuthenticatedUser`] (qui requiert `AuthState`)
// d'être utilisé dans un router parameteré par `WorkflowsState`.
impl FromRef<WorkflowsState> for AuthState {
    fn from_ref(input: &WorkflowsState) -> Self {
        input.auth.clone()
    }
}

/// Paramètres de query pour la pagination (`?offset=0&limit=50`).
#[derive(Debug, Deserialize, IntoParams)]
pub struct ListQuery {
    /// Offset, défaut 0.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Limite, défaut 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Réponse JSON de [`list_workflows`].
#[derive(Debug, Serialize, ToSchema)]
pub struct ListResponse {
    /// Items de la page (workflows complets — schéma libre côté OpenAPI).
    #[schema(value_type = Vec<Object>)]
    pub items: Vec<WorkflowGraph>,
    /// Total avant pagination.
    pub total: usize,
    /// Offset effectivement appliqué.
    pub offset: usize,
    /// Limit effectivement appliquée.
    pub limit: usize,
}

/// Erreur métier des handlers workflows. Mappée vers les codes HTTP standards.
#[derive(Debug)]
pub enum WorkflowError {
    /// Erreur d'auth/RBAC (propagation depuis le middleware).
    Auth(AuthError),
    /// Workflow non trouvé.
    NotFound(String),
    /// Conflit (id déjà existant).
    Conflict(String),
    /// Données invalides (body malformé, id mismatch).
    BadRequest(String),
    /// Erreur backend (repo).
    Internal(String),
}

impl From<AuthError> for WorkflowError {
    fn from(e: AuthError) -> Self {
        Self::Auth(e)
    }
}

impl IntoResponse for WorkflowError {
    fn into_response(self) -> Response {
        match self {
            WorkflowError::Auth(e) => e.into_response(),
            WorkflowError::NotFound(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
            WorkflowError::Conflict(msg) => (StatusCode::CONFLICT, msg).into_response(),
            WorkflowError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            WorkflowError::Internal(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        }
    }
}

fn audit_event(
    state: &WorkflowsState,
    actor: &str,
    action: &str,
    resource: &str,
    outcome: AuditOutcome,
) {
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let event = AuditEvent::new(now_ms, actor, action, outcome).with_resource(resource);
    if let Err(e) = state.auth.audit.record(event) {
        eprintln!("[workflows] audit record failed: {e}");
    }
}

/// `GET /workflows?offset=&limit=` — liste paginée (perm `workflows.read`).
#[utoipa::path(
    get,
    path = "/workflows",
    tag = "workflows",
    params(ListQuery),
    responses(
        (status = 200, description = "Page de workflows", body = ListResponse),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.read`")
    ),
    security(("bearer" = []))
)]
pub async fn list_workflows(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, WorkflowError> {
    state.auth.require_permission(&user, PERM_WORKFLOWS_READ)?;
    let paging = Pagination {
        offset: q.offset.unwrap_or(0),
        limit: q.limit.unwrap_or(50),
    };
    let WorkflowPage { items, total } = state
        .repo
        .list(paging)
        .map_err(|e| WorkflowError::Internal(e.to_string()))?;
    audit_event(
        &state,
        user.user_id(),
        "workflows.list",
        "*",
        AuditOutcome::Success,
    );
    Ok(Json(ListResponse {
        items,
        total,
        offset: paging.offset,
        limit: paging.limit,
    }))
}

/// `POST /workflows` — création (201 Created, perm `workflows.write`).
#[utoipa::path(
    post,
    path = "/workflows",
    tag = "workflows",
    request_body(content = serde_json::Value, description = "Workflow graph complet"),
    responses(
        (status = 201, description = "Créé", body = serde_json::Value),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.write`"),
        (status = 409, description = "Workflow déjà existant")
    ),
    security(("bearer" = []))
)]
pub async fn create_workflow(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Json(workflow): Json<WorkflowGraph>,
) -> Result<(StatusCode, Json<WorkflowGraph>), WorkflowError> {
    state.auth.require_permission(&user, PERM_WORKFLOWS_WRITE)?;
    let workflow_id = workflow.id.0.clone();
    match state.repo.create(workflow.clone()) {
        Ok(()) => {
            audit_event(
                &state,
                user.user_id(),
                "workflows.create",
                &workflow_id,
                AuditOutcome::Success,
            );
            Ok((StatusCode::CREATED, Json(workflow)))
        }
        Err(e) => {
            let msg = e.to_string();
            audit_event(
                &state,
                user.user_id(),
                "workflows.create",
                &workflow_id,
                AuditOutcome::Failure,
            );
            if msg.contains("déjà") {
                Err(WorkflowError::Conflict(msg))
            } else {
                Err(WorkflowError::Internal(msg))
            }
        }
    }
}

/// `GET /workflows/{id}` — détail (perm `workflows.read`).
#[utoipa::path(
    get,
    path = "/workflows/{id}",
    tag = "workflows",
    params(("id" = String, Path, description = "Identifiant du workflow")),
    responses(
        (status = 200, description = "Détail", body = serde_json::Value),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.read`"),
        (status = 404, description = "Workflow inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn get_workflow(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowGraph>, WorkflowError> {
    state.auth.require_permission(&user, PERM_WORKFLOWS_READ)?;
    match state
        .repo
        .get(&WorkflowId::new(&id))
        .map_err(|e| WorkflowError::Internal(e.to_string()))?
    {
        Some(w) => {
            audit_event(
                &state,
                user.user_id(),
                "workflows.get",
                &id,
                AuditOutcome::Success,
            );
            Ok(Json(w))
        }
        None => {
            audit_event(
                &state,
                user.user_id(),
                "workflows.get",
                &id,
                AuditOutcome::Failure,
            );
            Err(WorkflowError::NotFound(format!("workflow '{id}' inconnu")))
        }
    }
}

/// `PUT /workflows/{id}` — remplacement complet (perm `workflows.write`).
/// L'`id` du path doit correspondre à celui dans le body.
#[utoipa::path(
    put,
    path = "/workflows/{id}",
    tag = "workflows",
    params(("id" = String, Path, description = "Identifiant du workflow")),
    request_body(content = serde_json::Value, description = "Workflow graph complet"),
    responses(
        (status = 200, description = "Remplacé", body = serde_json::Value),
        (status = 400, description = "id path ≠ id body"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.write`"),
        (status = 404, description = "Workflow inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn update_workflow(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Path(id): Path<String>,
    Json(workflow): Json<WorkflowGraph>,
) -> Result<Json<WorkflowGraph>, WorkflowError> {
    state.auth.require_permission(&user, PERM_WORKFLOWS_WRITE)?;
    if workflow.id.0 != id {
        return Err(WorkflowError::BadRequest(format!(
            "id du path '{id}' != id du body '{}'",
            workflow.id.0
        )));
    }
    match state.repo.update(workflow.clone()) {
        Ok(()) => {
            audit_event(
                &state,
                user.user_id(),
                "workflows.update",
                &id,
                AuditOutcome::Success,
            );
            Ok(Json(workflow))
        }
        Err(e) => {
            let msg = e.to_string();
            audit_event(
                &state,
                user.user_id(),
                "workflows.update",
                &id,
                AuditOutcome::Failure,
            );
            if msg.contains("inconnu") {
                Err(WorkflowError::NotFound(msg))
            } else {
                Err(WorkflowError::Internal(msg))
            }
        }
    }
}

/// `DELETE /workflows/{id}` — suppression (204, idempotent, perm `workflows.write`).
#[utoipa::path(
    delete,
    path = "/workflows/{id}",
    tag = "workflows",
    params(("id" = String, Path, description = "Identifiant du workflow")),
    responses(
        (status = 204, description = "Supprimé (idempotent)"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.write`")
    ),
    security(("bearer" = []))
)]
pub async fn delete_workflow(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Path(id): Path<String>,
) -> Result<StatusCode, WorkflowError> {
    state.auth.require_permission(&user, PERM_WORKFLOWS_WRITE)?;
    let _existed = state
        .repo
        .delete(&WorkflowId::new(&id))
        .map_err(|e| WorkflowError::Internal(e.to_string()))?;
    audit_event(
        &state,
        user.user_id(),
        "workflows.delete",
        &id,
        AuditOutcome::Success,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Construit le sous-router `/workflows/*`. Composable via `Router::nest`.
pub fn workflows_router(state: WorkflowsState) -> Router {
    Router::new()
        .route("/", get(list_workflows).post(create_workflow))
        .route(
            "/:id",
            get(get_workflow)
                .put(update_workflow)
                .delete(delete_workflow),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{
        AuditSink, InMemoryAuditSink, InMemoryPermissionStore, InMemoryRevocationStore, NodeId,
        NodeKind, PermissionStore, RevocationStore, Role, TokenService,
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
        WorkflowsState,
        TokenService,
        Arc<InMemoryPermissionStore>,
        Arc<InMemoryAuditSink>,
        Arc<dyn WorkflowRepository + Send + Sync>,
    ) {
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let perms = Arc::new(InMemoryPermissionStore::new());
        let audit = Arc::new(InMemoryAuditSink::new());
        let repo: Arc<dyn WorkflowRepository + Send + Sync> =
            Arc::new(autome_core::InMemoryWorkflowRepository::new());
        let svc = TokenService::new(SECRET);
        let auth = AuthState::new(
            TokenService::new(SECRET),
            revocation,
            Arc::clone(&perms) as Arc<dyn PermissionStore + Send + Sync>,
            Arc::clone(&audit) as Arc<dyn AuditSink + Send + Sync>,
        )
        .with_clock(Arc::new(FixedClock));
        let state = WorkflowsState::new(auth, Arc::clone(&repo));
        (state, svc, perms, audit, repo)
    }

    fn sample_workflow_json(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": format!("workflow {id}"),
            "nodes": [
                {
                    "id": "entry",
                    "name": "entry",
                    "kind": "trigger",
                    "trigger_type": "manual"
                }
            ],
            "edges": []
        })
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

    // ───── F7.1.b/c — chemins nominaux ─────

    #[tokio::test]
    async fn list_returns_200_with_empty_page() {
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("GET", "/", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["total"], 0);
        assert_eq!(body["items"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn create_returns_201_and_persists() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let payload = sample_workflow_json("wf-1").to_string();
        let response = app
            .oneshot(auth_req("POST", "/", &token, Some(&payload)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        // Repo a bien stocké
        assert!(repo.get(&WorkflowId::new("wf-1")).unwrap().is_some());
    }

    #[tokio::test]
    async fn get_returns_200_when_workflow_exists() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        repo.create(sample_workflow("wf-1")).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("GET", "/wf-1", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["id"], "wf-1");
    }

    #[tokio::test]
    async fn get_returns_404_when_unknown() {
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("GET", "/missing", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_returns_200_with_updated_workflow() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        repo.create(sample_workflow("wf-1")).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let mut new_payload = sample_workflow_json("wf-1");
        new_payload["name"] = serde_json::json!("renamed");
        let response = app
            .oneshot(auth_req(
                "PUT",
                "/wf-1",
                &token,
                Some(&new_payload.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            repo.get(&WorkflowId::new("wf-1")).unwrap().unwrap().name,
            "renamed"
        );
    }

    #[tokio::test]
    async fn delete_returns_204_and_removes_workflow() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        repo.create(sample_workflow("wf-1")).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("DELETE", "/wf-1", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(repo.get(&WorkflowId::new("wf-1")).unwrap().is_none());
    }

    // ───── erreurs métier ─────

    #[tokio::test]
    async fn create_duplicate_returns_409() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        repo.create(sample_workflow("wf-1")).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let payload = sample_workflow_json("wf-1").to_string();
        let response = app
            .oneshot(auth_req("POST", "/", &token, Some(&payload)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn update_unknown_returns_404() {
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let payload = sample_workflow_json("missing").to_string();
        let response = app
            .oneshot(auth_req("PUT", "/missing", &token, Some(&payload)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_with_mismatched_id_returns_400() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        repo.create(sample_workflow("wf-1")).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        // body id != path id
        let payload = sample_workflow_json("wf-other").to_string();
        let response = app
            .oneshot(auth_req("PUT", "/wf-1", &token, Some(&payload)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_unknown_returns_204_idempotent() {
        // Le DELETE est idempotent — supprimer un workflow inexistant est OK.
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("DELETE", "/missing", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    // ───── F7.1.c — RBAC ─────

    #[tokio::test]
    async fn list_returns_401_without_auth() {
        let (state, _, _, _, _) = make_test_state();
        let app = workflows_router(state);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn create_returns_403_without_write_perm() {
        // Viewer n'a que workflows.read → 403 sur POST
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let payload = sample_workflow_json("wf-1").to_string();
        let response = app
            .oneshot(auth_req("POST", "/", &token, Some(&payload)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn list_returns_403_without_read_perm() {
        // Aucun rôle → 403 sur GET
        let (state, svc, _, _, _) = make_test_state();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("GET", "/", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ───── F7.1.c — Audit ─────

    #[tokio::test]
    async fn successful_create_records_audit_event_with_workflow_id() {
        let (state, svc, perms, audit, _) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let payload = sample_workflow_json("wf-7").to_string();
        let _ = app
            .oneshot(auth_req("POST", "/", &token, Some(&payload)))
            .await
            .unwrap();
        let events = audit.list().unwrap();
        // Un event pour permission_check (Success) + un pour workflows.create
        let create_event = events
            .iter()
            .find(|e| e.action == "workflows.create")
            .expect("workflows.create event présent");
        assert_eq!(create_event.actor, "alice");
        assert_eq!(create_event.outcome, AuditOutcome::Success);
        assert_eq!(create_event.resource.as_deref(), Some("wf-7"));
    }

    #[tokio::test]
    async fn list_pagination_query_params_work() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        for i in 0..5 {
            repo.create(sample_workflow(&format!("wf-{i}"))).unwrap();
        }
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("GET", "/?offset=2&limit=2", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["total"], 5);
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        assert_eq!(body["offset"], 2);
        assert_eq!(body["limit"], 2);
    }
}

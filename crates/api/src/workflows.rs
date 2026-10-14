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
//!
//! **Isolation par propriétaire** (backlog BOLA) : un workflow appartient à
//! l'utilisateur qui l'a créé. La liste est filtrée sur lui ; lire, modifier
//! ou supprimer le workflow d'un autre répond **comme s'il n'existait pas**
//! (404, ou 204 pour le DELETE idempotent) — pas de 403 qui confirmerait son
//! existence. Seul un super-admin (`*`) voit tous les workflows.

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

use crate::auth::{AuthError, AuthState, AuthenticatedUser, OwnerScope};

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

/// `true` si `user` peut accéder au workflow `id` : il en est propriétaire, ou
/// il est super-admin. Un workflow inconnu est inaccessible à un utilisateur
/// ordinaire (même réponse qu'un workflow d'autrui).
async fn can_access(
    state: &WorkflowsState,
    user: &AuthenticatedUser,
    id: &str,
) -> Result<bool, WorkflowError> {
    let scope = state.auth.owner_scope(user).await?;
    if scope == OwnerScope::All {
        return Ok(true);
    }
    let owner = state
        .repo
        .owner_of(&WorkflowId::new(id))
        .await
        .map_err(|e| WorkflowError::Internal(e.to_string()))?;
    Ok(scope.allows(owner.as_deref()))
}

async fn audit_event(
    state: &WorkflowsState,
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
    state
        .auth
        .require_permission(&user, PERM_WORKFLOWS_READ)
        .await?;
    let paging = Pagination {
        offset: q.offset.unwrap_or(0),
        limit: q.limit.unwrap_or(50),
    };
    let scope = state.auth.owner_scope(&user).await?;
    let WorkflowPage { items, total } = state
        .repo
        .list(scope.filter(), paging)
        .await
        .map_err(|e| WorkflowError::Internal(e.to_string()))?;
    audit_event(
        &state,
        user.user_id(),
        "workflows.list",
        "*",
        AuditOutcome::Success,
    )
    .await;
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
    state
        .auth
        .require_permission(&user, PERM_WORKFLOWS_WRITE)
        .await?;
    let workflow_id = workflow.id.0.clone();
    // Le créateur devient propriétaire (y compris un super-admin).
    match state
        .repo
        .create(workflow.clone(), Some(user.user_id()))
        .await
    {
        Ok(()) => {
            audit_event(
                &state,
                user.user_id(),
                "workflows.create",
                &workflow_id,
                AuditOutcome::Success,
            )
            .await;
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
            )
            .await;
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
    state
        .auth
        .require_permission(&user, PERM_WORKFLOWS_READ)
        .await?;
    if !can_access(&state, &user, &id).await? {
        audit_event(
            &state,
            user.user_id(),
            "workflows.get",
            &id,
            AuditOutcome::Denied,
        )
        .await;
        return Err(WorkflowError::NotFound(format!("workflow '{id}' inconnu")));
    }
    match state
        .repo
        .get(&WorkflowId::new(&id))
        .await
        .map_err(|e| WorkflowError::Internal(e.to_string()))?
    {
        Some(w) => {
            audit_event(
                &state,
                user.user_id(),
                "workflows.get",
                &id,
                AuditOutcome::Success,
            )
            .await;
            Ok(Json(w))
        }
        None => {
            audit_event(
                &state,
                user.user_id(),
                "workflows.get",
                &id,
                AuditOutcome::Failure,
            )
            .await;
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
    state
        .auth
        .require_permission(&user, PERM_WORKFLOWS_WRITE)
        .await?;
    if workflow.id.0 != id {
        return Err(WorkflowError::BadRequest(format!(
            "id du path '{id}' != id du body '{}'",
            workflow.id.0
        )));
    }
    if !can_access(&state, &user, &id).await? {
        audit_event(
            &state,
            user.user_id(),
            "workflows.update",
            &id,
            AuditOutcome::Denied,
        )
        .await;
        return Err(WorkflowError::NotFound(format!(
            "workflow : id '{id}' inconnu"
        )));
    }
    match state.repo.update(workflow.clone()).await {
        Ok(()) => {
            audit_event(
                &state,
                user.user_id(),
                "workflows.update",
                &id,
                AuditOutcome::Success,
            )
            .await;
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
            )
            .await;
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
    state
        .auth
        .require_permission(&user, PERM_WORKFLOWS_WRITE)
        .await?;
    // Workflow d'autrui : 204 sans rien supprimer — même réponse qu'un id
    // inconnu (DELETE idempotent), l'existence n'est pas révélée.
    if !can_access(&state, &user, &id).await? {
        audit_event(
            &state,
            user.user_id(),
            "workflows.delete",
            &id,
            AuditOutcome::Denied,
        )
        .await;
        return Ok(StatusCode::NO_CONTENT);
    }
    let _existed = state
        .repo
        .delete(&WorkflowId::new(&id))
        .await
        .map_err(|e| WorkflowError::Internal(e.to_string()))?;
    audit_event(
        &state,
        user.user_id(),
        "workflows.delete",
        &id,
        AuditOutcome::Success,
    )
    .await;
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
        assert!(repo.get(&WorkflowId::new("wf-1")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn get_returns_200_when_workflow_exists() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        repo.create(sample_workflow("wf-1"), Some("alice"))
            .await
            .unwrap();
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
        repo.create(sample_workflow("wf-1"), Some("alice"))
            .await
            .unwrap();
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
            repo.get(&WorkflowId::new("wf-1"))
                .await
                .unwrap()
                .unwrap()
                .name,
            "renamed"
        );
    }

    #[tokio::test]
    async fn delete_returns_204_and_removes_workflow() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        repo.create(sample_workflow("wf-1"), Some("alice"))
            .await
            .unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("DELETE", "/wf-1", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(repo.get(&WorkflowId::new("wf-1")).await.unwrap().is_none());
    }

    // ───── erreurs métier ─────

    #[tokio::test]
    async fn create_duplicate_returns_409() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        repo.create(sample_workflow("wf-1"), Some("alice"))
            .await
            .unwrap();
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
        repo.create(sample_workflow("wf-1"), Some("alice"))
            .await
            .unwrap();
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
        let events = audit.list().await.unwrap();
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
            repo.create(sample_workflow(&format!("wf-{i}")), Some("alice"))
                .await
                .unwrap();
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

    // ───── Backlog BOLA — isolation par propriétaire ─────

    /// État avec alice et bob (editor) + root (super-admin), et `wf-a`
    /// appartenant à alice.
    async fn two_users() -> (
        Router,
        TokenService,
        Arc<InMemoryAuditSink>,
        Arc<dyn WorkflowRepository + Send + Sync>,
    ) {
        let (state, svc, perms, audit, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        perms.assign_role("bob", &Role::editor()).unwrap();
        perms.assign_role("root", &Role::super_admin()).unwrap();
        repo.create(sample_workflow("wf-a"), Some("alice"))
            .await
            .unwrap();
        (workflows_router(state), svc, audit, repo)
    }

    fn token(svc: &TokenService, sub: &str) -> String {
        svc.issue_access(sub, 900, FIXED_NOW).unwrap()
    }

    #[tokio::test]
    async fn create_records_the_caller_as_owner() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let payload = sample_workflow_json("wf-new").to_string();
        let r = workflows_router(state)
            .oneshot(auth_req("POST", "/", &token(&svc, "alice"), Some(&payload)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(
            repo.owner_of(&WorkflowId::new("wf-new"))
                .await
                .unwrap()
                .as_deref(),
            Some("alice")
        );
    }

    #[tokio::test]
    async fn list_only_returns_the_callers_workflows() {
        let (app, svc, _, repo) = two_users().await;
        repo.create(sample_workflow("wf-b"), Some("bob"))
            .await
            .unwrap();
        repo.create(sample_workflow("wf-legacy"), None)
            .await
            .unwrap();
        let ids = |body: String| -> Vec<String> {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w["id"].as_str().unwrap().to_string())
                .collect()
        };
        let bob = app
            .clone()
            .oneshot(auth_req("GET", "/", &token(&svc, "bob"), None))
            .await
            .unwrap();
        assert_eq!(ids(body_string(bob).await), vec!["wf-b"]);
        let root = app
            .oneshot(auth_req("GET", "/", &token(&svc, "root"), None))
            .await
            .unwrap();
        assert_eq!(ids(body_string(root).await).len(), 3, "super-admin : tout");
    }

    #[tokio::test]
    async fn cannot_read_someone_elses_workflow() {
        let (app, svc, audit, _) = two_users().await;
        let r = app
            .clone()
            .oneshot(auth_req("GET", "/wf-a", &token(&svc, "bob"), None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        // Même réponse que pour un id inconnu : l'existence n'est pas révélée.
        let unknown = app
            .oneshot(auth_req("GET", "/wf-zzz", &token(&svc, "bob"), None))
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_string(r).await.replace("wf-a", "ID"),
            body_string(unknown).await.replace("wf-zzz", "ID")
        );
        let events = audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "workflows.get" && e.outcome == AuditOutcome::Denied));
    }

    #[tokio::test]
    async fn cannot_update_someone_elses_workflow() {
        let (app, svc, _, repo) = two_users().await;
        let mut hijack = sample_workflow_json("wf-a");
        hijack["name"] = serde_json::json!("pwned");
        let r = app
            .oneshot(auth_req(
                "PUT",
                "/wf-a",
                &token(&svc, "bob"),
                Some(&hijack.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let kept = repo.get(&WorkflowId::new("wf-a")).await.unwrap().unwrap();
        assert_eq!(kept.name, "workflow wf-a");
    }

    #[tokio::test]
    async fn cannot_delete_someone_elses_workflow() {
        let (app, svc, audit, repo) = two_users().await;
        let r = app
            .oneshot(auth_req("DELETE", "/wf-a", &token(&svc, "bob"), None))
            .await
            .unwrap();
        // 204 comme pour un id inconnu, mais rien n'est supprimé.
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(repo.get(&WorkflowId::new("wf-a")).await.unwrap().is_some());
        let events = audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "workflows.delete" && e.outcome == AuditOutcome::Denied));
    }

    #[tokio::test]
    async fn owner_and_super_admin_keep_full_access() {
        let (app, svc, _, repo) = two_users().await;
        for who in ["alice", "root"] {
            let r = app
                .clone()
                .oneshot(auth_req("GET", "/wf-a", &token(&svc, who), None))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK, "{who} doit lire wf-a");
        }
        let r = app
            .oneshot(auth_req("DELETE", "/wf-a", &token(&svc, "root"), None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(repo.get(&WorkflowId::new("wf-a")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn workflow_without_owner_is_hidden_from_regular_users() {
        let (app, svc, _, repo) = two_users().await;
        repo.create(sample_workflow("wf-legacy"), None)
            .await
            .unwrap();
        let r = app
            .oneshot(auth_req("GET", "/wf-legacy", &token(&svc, "alice"), None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}

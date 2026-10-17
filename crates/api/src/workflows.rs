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
//! [`autome_core::AuditSink`] avec actor + resource = workflow_id
//! (`<owner>/<id>` quand un super-admin vise l'espace d'autrui).
//!
//! **Espaces d'ids par propriétaire** (#188) : chaque utilisateur a son propre
//! espace d'ids de workflow ; toutes les routes opèrent dans celui de
//! l'appelant. Le workflow d'un autre n'y existe pas : le lire répond 404, et
//! créer le même id **réussit** (409 seulement pour un doublon dans son propre
//! espace) — l'existence des workflows d'autrui n'est jamais révélée. Un
//! super-admin (`*`) vise l'espace d'un autre avec `?owner=<user_id>` (vide :
//! workflows antérieurs à la migration `0010`, sans propriétaire) ; un
//! utilisateur ordinaire qui le tente reçoit 403, quelle que soit l'existence
//! de la ressource.
//!
//! **Déclencheurs planifiés** (#227) : à la création et à la mise à jour, les
//! paramètres des nœuds `schedule` / `polling` sont validés (400 qui nomme le
//! nœud : cron invalide, fuseau autre qu'UTC, intervalle hors bornes, URL non
//! http(s)…) puis leurs échéances alignées sur le workflow enregistré
//! ([`autome_core::TriggerScheduleStore::sync`]). Si cet alignement échoue, la
//! requête répond 500 : une création est annulée (workflow supprimé, un nouvel
//! essai réussit), une mise à jour peut être renvoyée telle quelle.

use std::sync::Arc;

use autome_connectors::planned_triggers;
use autome_core::{
    AuditEvent, AuditOutcome, Pagination, ScheduledTrigger, TriggerScheduleStore,
    WebhookEndpointStore, WorkflowGraph, WorkflowId, WorkflowPage, WorkflowRepository,
    PERM_WORKFLOWS_READ, PERM_WORKFLOWS_WRITE,
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
    /// Endpoints de webhook (#202), purgés quand un nœud webhook ou le
    /// workflow disparaît ; `None` = pas de purge (tests).
    pub webhooks: Option<Arc<dyn WebhookEndpointStore>>,
    /// Échéances des déclencheurs planifiés (#227) ; `None` = non tenues
    /// (tests). Les paramètres sont validés dans tous les cas.
    pub schedules: Option<Arc<dyn TriggerScheduleStore>>,
}

impl WorkflowsState {
    /// Builder convenience.
    pub fn new(auth: AuthState, repo: Arc<dyn WorkflowRepository + Send + Sync>) -> Self {
        Self {
            auth,
            repo,
            webhooks: None,
            schedules: None,
        }
    }

    /// Builder : échéances des déclencheurs planifiés (#227).
    pub fn with_schedules(mut self, schedules: Arc<dyn TriggerScheduleStore>) -> Self {
        self.schedules = Some(schedules);
        self
    }

    /// Aligne les échéances du workflow sur ses nœuds planifiés.
    async fn sync_schedules(
        &self,
        ns: &str,
        id: &str,
        triggers: &[ScheduledTrigger],
    ) -> Result<(), String> {
        let Some(schedules) = &self.schedules else {
            return Ok(());
        };
        schedules
            .sync(ns, id, triggers)
            .await
            .map_err(|e| e.to_string())
    }

    /// Builder : purge des endpoints de webhook (#202).
    pub fn with_webhooks(mut self, webhooks: Arc<dyn WebhookEndpointStore>) -> Self {
        self.webhooks = Some(webhooks);
        self
    }

    /// Supprime les URL de webhook des nœuds absents de `keep`. Un échec est
    /// journalisé sans faire échouer la requête : une URL orpheline répond
    /// déjà 404 (son nœud n'existe plus).
    async fn prune_webhooks(&self, ns: &str, id: &str, keep: &[String]) {
        let Some(webhooks) = &self.webhooks else {
            return;
        };
        if let Err(e) = webhooks.retain(ns, id, keep).await {
            tracing::warn!(workflow_id = id, error = %e, "purge des URL de webhook impossible");
        }
    }
}

// Permet à l'extractor [`AuthenticatedUser`] (qui requiert `AuthState`)
// d'être utilisé dans un router parameteré par `WorkflowsState`.
impl FromRef<WorkflowsState> for AuthState {
    fn from_ref(input: &WorkflowsState) -> Self {
        input.auth.clone()
    }
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
    /// Espace d'un autre utilisateur (super-admin seulement) — cf.
    /// [`NamespaceQuery`].
    #[serde(default)]
    pub owner: Option<String>,
}

/// Espace d'ids visé (#188) : absent = celui de l'appelant. Un autre espace
/// (`?owner=<user_id>`, vide = workflows sans propriétaire) est réservé aux
/// super-admins (403 sinon).
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct NamespaceQuery {
    /// Propriétaire de l'espace visé.
    #[serde(default)]
    pub owner: Option<String>,
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

/// Nœuds planifiés de `workflow`, paramètres validés (400 sinon).
fn validated_triggers(workflow: &WorkflowGraph) -> Result<Vec<ScheduledTrigger>, WorkflowError> {
    planned_triggers(workflow).map_err(WorkflowError::BadRequest)
}

/// Ressource d'audit : l'id, préfixé du propriétaire quand l'espace visé
/// n'est pas celui de l'appelant (super-admin).
fn resource(user: &AuthenticatedUser, ns: &str, id: &str) -> String {
    if ns == user.user_id() {
        id.to_string()
    } else {
        format!("{ns}/{id}")
    }
}

fn internal(e: impl std::fmt::Display) -> WorkflowError {
    WorkflowError::Internal(e.to_string())
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
        (status = 403, description = "Manque `workflows.read`, ou `owner` d'autrui sans `*`")
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
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let WorkflowPage { items, total } = state.repo.list(&ns, paging).await.map_err(internal)?;
    audit_event(
        &state,
        user.user_id(),
        "workflows.list",
        &resource(&user, &ns, "*"),
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

/// `POST /workflows` — création (201 Created, perm `workflows.write`) dans
/// l'espace de l'appelant (ou `?owner=` pour un super-admin).
#[utoipa::path(
    post,
    path = "/workflows",
    tag = "workflows",
    params(NamespaceQuery),
    request_body(content = serde_json::Value, description = "Workflow graph complet"),
    responses(
        (status = 201, description = "Créé", body = serde_json::Value),
        (status = 400, description = "Paramètres d'un nœud `schedule` / `polling` invalides"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.write`, ou `owner` d'autrui sans `*`"),
        (status = 409, description = "Id déjà utilisé dans cet espace")
    ),
    security(("bearer" = []))
)]
pub async fn create_workflow(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Query(q): Query<NamespaceQuery>,
    Json(workflow): Json<WorkflowGraph>,
) -> Result<(StatusCode, Json<WorkflowGraph>), WorkflowError> {
    state
        .auth
        .require_permission(&user, PERM_WORKFLOWS_WRITE)
        .await?;
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let triggers = validated_triggers(&workflow)?;
    let res = resource(&user, &ns, &workflow.id.0);
    match state.repo.create(&ns, workflow.clone()).await {
        Ok(()) => {
            if let Err(e) = state.sync_schedules(&ns, &workflow.id.0, &triggers).await {
                // Annulée : un nouvel essai ne doit pas répondre 409.
                if let Err(rollback) = state.repo.delete(&ns, &workflow.id).await {
                    tracing::error!(workflow_id = %workflow.id.0, error = %rollback,
                        "création : annulation impossible après l'échec des échéances");
                }
                audit_event(
                    &state,
                    user.user_id(),
                    "workflows.create",
                    &res,
                    AuditOutcome::Failure,
                )
                .await;
                return Err(WorkflowError::Internal(format!(
                    "échéances des déclencheurs non enregistrées : {e}"
                )));
            }
            audit_event(
                &state,
                user.user_id(),
                "workflows.create",
                &res,
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
                &res,
                AuditOutcome::Failure,
            )
            .await;
            // Doublon dans l'espace visé seulement : l'id d'autrui est libre.
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
    params(("id" = String, Path, description = "Identifiant du workflow"), NamespaceQuery),
    responses(
        (status = 200, description = "Détail", body = serde_json::Value),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.read`, ou `owner` d'autrui sans `*`"),
        (status = 404, description = "Workflow inconnu dans cet espace")
    ),
    security(("bearer" = []))
)]
pub async fn get_workflow(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Path(id): Path<String>,
    Query(q): Query<NamespaceQuery>,
) -> Result<Json<WorkflowGraph>, WorkflowError> {
    state
        .auth
        .require_permission(&user, PERM_WORKFLOWS_READ)
        .await?;
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let found = state
        .repo
        .get(&ns, &WorkflowId::new(&id))
        .await
        .map_err(internal)?;
    let outcome = if found.is_some() {
        AuditOutcome::Success
    } else {
        AuditOutcome::Failure
    };
    audit_event(
        &state,
        user.user_id(),
        "workflows.get",
        &resource(&user, &ns, &id),
        outcome,
    )
    .await;
    found
        .map(Json)
        .ok_or_else(|| WorkflowError::NotFound(format!("workflow '{id}' inconnu")))
}

/// `PUT /workflows/{id}` — remplacement complet (perm `workflows.write`).
/// L'`id` du path doit correspondre à celui dans le body.
#[utoipa::path(
    put,
    path = "/workflows/{id}",
    tag = "workflows",
    params(("id" = String, Path, description = "Identifiant du workflow"), NamespaceQuery),
    request_body(content = serde_json::Value, description = "Workflow graph complet"),
    responses(
        (status = 200, description = "Remplacé", body = serde_json::Value),
        (status = 400, description = "id path ≠ id body, ou paramètres d'un nœud `schedule` / `polling` invalides"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.write`, ou `owner` d'autrui sans `*`"),
        (status = 404, description = "Workflow inconnu dans cet espace")
    ),
    security(("bearer" = []))
)]
pub async fn update_workflow(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Path(id): Path<String>,
    Query(q): Query<NamespaceQuery>,
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
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let triggers = validated_triggers(&workflow)?;
    let res = resource(&user, &ns, &id);
    match state.repo.update(&ns, workflow.clone()).await {
        Ok(()) => {
            state
                .prune_webhooks(&ns, &id, &crate::webhooks::webhook_node_ids(&workflow))
                .await;
            if let Err(e) = state.sync_schedules(&ns, &id, &triggers).await {
                audit_event(
                    &state,
                    user.user_id(),
                    "workflows.update",
                    &res,
                    AuditOutcome::Failure,
                )
                .await;
                return Err(WorkflowError::Internal(format!(
                    "workflow enregistré mais échéances des déclencheurs non alignées \
                     (renvoyer la requête) : {e}"
                )));
            }
            audit_event(
                &state,
                user.user_id(),
                "workflows.update",
                &res,
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
                &res,
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
    params(("id" = String, Path, description = "Identifiant du workflow"), NamespaceQuery),
    responses(
        (status = 204, description = "Supprimé (idempotent)"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `workflows.write`, ou `owner` d'autrui sans `*`")
    ),
    security(("bearer" = []))
)]
pub async fn delete_workflow(
    user: AuthenticatedUser,
    State(state): State<WorkflowsState>,
    Path(id): Path<String>,
    Query(q): Query<NamespaceQuery>,
) -> Result<StatusCode, WorkflowError> {
    state
        .auth
        .require_permission(&user, PERM_WORKFLOWS_WRITE)
        .await?;
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let existed = state
        .repo
        .delete(&ns, &WorkflowId::new(&id))
        .await
        .map_err(internal)?;
    state.prune_webhooks(&ns, &id, &[]).await;
    // En PostgreSQL la suppression du workflow emporte ses échéances
    // (cascade) ; ici pour les autres stores.
    if let Err(e) = state.sync_schedules(&ns, &id, &[]).await {
        tracing::warn!(workflow_id = %id, error = %e, "suppression des échéances impossible");
    }
    let outcome = if existed {
        AuditOutcome::Success
    } else {
        AuditOutcome::Failure
    };
    audit_event(
        &state,
        user.user_id(),
        "workflows.delete",
        &resource(&user, &ns, &id),
        outcome,
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
        NodeKind, PermissionStore, RevocationStore, Role, TokenService, LEGACY_NAMESPACE,
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
                position: None,
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
        assert!(repo
            .get("alice", &WorkflowId::new("wf-1"))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn get_returns_200_when_workflow_exists() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        repo.create("alice", sample_workflow("wf-1")).await.unwrap();
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
        repo.create("alice", sample_workflow("wf-1")).await.unwrap();
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
            repo.get("alice", &WorkflowId::new("wf-1"))
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
        repo.create("alice", sample_workflow("wf-1")).await.unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = workflows_router(state);
        let response = app
            .oneshot(auth_req("DELETE", "/wf-1", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(repo
            .get("alice", &WorkflowId::new("wf-1"))
            .await
            .unwrap()
            .is_none());
    }

    // ───── erreurs métier ─────

    #[tokio::test]
    async fn create_duplicate_returns_409() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        repo.create("alice", sample_workflow("wf-1")).await.unwrap();
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
        repo.create("alice", sample_workflow("wf-1")).await.unwrap();
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
            repo.create("alice", sample_workflow(&format!("wf-{i}")))
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

    // ───── Espaces d'ids par propriétaire (#188) ─────

    /// État avec alice et bob (editor) + root (super-admin), et `wf-a` dans
    /// l'espace d'alice.
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
        repo.create("alice", sample_workflow("wf-a")).await.unwrap();
        (workflows_router(state), svc, audit, repo)
    }

    fn token(svc: &TokenService, sub: &str) -> String {
        svc.issue_access(sub, 900, FIXED_NOW).unwrap()
    }

    fn wid(id: &str) -> WorkflowId {
        WorkflowId::new(id)
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        who: &str,
        svc: &TokenService,
    ) -> Response {
        app.clone()
            .oneshot(auth_req(method, uri, &token(svc, who), None))
            .await
            .unwrap()
    }

    fn ids(body: String) -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn create_stores_in_the_callers_namespace() {
        let (state, svc, perms, _, repo) = make_test_state();
        perms.assign_role("alice", &Role::editor()).unwrap();
        let payload = sample_workflow_json("wf-new").to_string();
        let r = workflows_router(state)
            .oneshot(auth_req("POST", "/", &token(&svc, "alice"), Some(&payload)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert!(repo.get("alice", &wid("wf-new")).await.unwrap().is_some());
        assert!(repo.get("bob", &wid("wf-new")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_id_taken_by_someone_else_is_free_no_409() {
        let (app, svc, _, repo) = two_users().await;
        let mut mine = sample_workflow_json("wf-a");
        mine["name"] = serde_json::json!("wf-a de bob");
        let create = |body: String| {
            let (app, t) = (app.clone(), token(&svc, "bob"));
            async move {
                app.oneshot(auth_req("POST", "/", &t, Some(&body)))
                    .await
                    .unwrap()
                    .status()
            }
        };
        // Avant #188 : 409, qui révélait le workflow d'alice.
        assert_eq!(create(mine.to_string()).await, StatusCode::CREATED);
        // Le doublon dans son propre espace reste un conflit.
        assert_eq!(create(mine.to_string()).await, StatusCode::CONFLICT);

        let bob = call(&app, "GET", "/wf-a", "bob", &svc).await;
        let v: serde_json::Value = serde_json::from_str(&body_string(bob).await).unwrap();
        assert_eq!(v["name"], "wf-a de bob");
        let alice = repo.get("alice", &wid("wf-a")).await.unwrap().unwrap();
        assert_eq!(
            alice.name, "workflow wf-a",
            "le workflow d'alice est intact"
        );
    }

    #[tokio::test]
    async fn list_shows_only_the_callers_namespace() {
        let (app, svc, _, repo) = two_users().await;
        repo.create("bob", sample_workflow("wf-b")).await.unwrap();
        repo.create(LEGACY_NAMESPACE, sample_workflow("wf-legacy"))
            .await
            .unwrap();
        let list = |uri: &'static str, who: &'static str| {
            let (app, svc) = (app.clone(), &svc);
            async move { ids(body_string(call(&app, "GET", uri, who, svc).await).await) }
        };
        assert_eq!(list("/", "bob").await, vec!["wf-b"]);
        // Le super-admin a lui aussi son propre espace (vide ici)…
        assert!(list("/", "root").await.is_empty());
        // … et vise celui d'un autre explicitement.
        assert_eq!(list("/?owner=alice", "root").await, vec!["wf-a"]);
        assert_eq!(list("/?owner=", "root").await, vec!["wf-legacy"]);
    }

    #[tokio::test]
    async fn someone_elses_workflow_does_not_exist_in_my_namespace() {
        let (app, svc, audit, repo) = two_users().await;
        let r = call(&app, "GET", "/wf-a", "bob", &svc).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        // Même réponse que pour un id inconnu : l'existence n'est pas révélée.
        let unknown = call(&app, "GET", "/wf-zzz", "bob", &svc).await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_string(r).await.replace("wf-a", "ID"),
            body_string(unknown).await.replace("wf-zzz", "ID")
        );

        let mut hijack = sample_workflow_json("wf-a");
        hijack["name"] = serde_json::json!("pwned");
        let put = app
            .clone()
            .oneshot(auth_req(
                "PUT",
                "/wf-a",
                &token(&svc, "bob"),
                Some(&hijack.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::NOT_FOUND);
        // DELETE idempotent : 204, mais rien n'est supprimé chez alice.
        let del = call(&app, "DELETE", "/wf-a", "bob", &svc).await;
        assert_eq!(del.status(), StatusCode::NO_CONTENT);
        let kept = repo.get("alice", &wid("wf-a")).await.unwrap().unwrap();
        assert_eq!(kept.name, "workflow wf-a");

        let events = audit.list().await.unwrap();
        for action in ["workflows.get", "workflows.update", "workflows.delete"] {
            assert!(events.iter().any(|e| e.action == action
                && e.actor == "bob"
                && e.outcome == AuditOutcome::Failure));
        }
    }

    #[tokio::test]
    async fn targeting_another_namespace_requires_super_admin() {
        let (app, svc, _, _) = two_users().await;
        // 403 que le workflow existe ou non : rien n'est révélé.
        for uri in [
            "/wf-a?owner=alice",
            "/wf-zzz?owner=alice",
            "/?owner=alice",
            "/wf-legacy?owner=",
        ] {
            let r = call(&app, "GET", uri, "bob", &svc).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{uri}");
        }
        // Viser explicitement son propre espace est permis.
        assert_eq!(
            call(&app, "GET", "/wf-a?owner=alice", "alice", &svc)
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn super_admin_reaches_another_namespace_with_owner() {
        let (app, svc, audit, repo) = two_users().await;
        assert_eq!(
            call(&app, "GET", "/wf-a", "root", &svc).await.status(),
            StatusCode::NOT_FOUND,
            "sans `owner`, root est dans son propre espace"
        );
        assert_eq!(
            call(&app, "GET", "/wf-a?owner=alice", "root", &svc)
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            call(&app, "DELETE", "/wf-a?owner=alice", "root", &svc)
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert!(repo.get("alice", &wid("wf-a")).await.unwrap().is_none());
        let events = audit.list().await.unwrap();
        assert!(events.iter().any(|e| e.action == "workflows.delete"
            && e.actor == "root"
            && e.resource.as_deref() == Some("alice/wf-a")
            && e.outcome == AuditOutcome::Success));
    }

    #[tokio::test]
    async fn workflows_without_owner_are_reserved_to_super_admins() {
        let (app, svc, _, repo) = two_users().await;
        repo.create(LEGACY_NAMESPACE, sample_workflow("wf-legacy"))
            .await
            .unwrap();
        assert_eq!(
            call(&app, "GET", "/wf-legacy", "alice", &svc)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, "GET", "/wf-legacy?owner=", "root", &svc)
                .await
                .status(),
            StatusCode::OK
        );
    }

    // ───── #202 — purge des URL de webhook ─────

    #[tokio::test]
    async fn removing_a_webhook_node_or_the_workflow_drops_its_urls() {
        use autome_core::{EndpointSecret, InMemoryWebhookEndpointStore, WebhookEndpointStore};

        let (state, svc, perms, _, repo) = make_test_state();
        let webhooks = Arc::new(InMemoryWebhookEndpointStore::new());
        let app = workflows_router(state.with_webhooks(webhooks.clone()));
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();

        let mut wf = sample_workflow_json("wf-1");
        wf["nodes"].as_array_mut().unwrap().extend([
            serde_json::json!({ "id": "a", "name": "a", "kind": "trigger", "trigger_type": "webhook" }),
            serde_json::json!({ "id": "b", "name": "b", "kind": "trigger", "trigger_type": "webhook" }),
        ]);
        repo.create("alice", serde_json::from_value(wf.clone()).unwrap())
            .await
            .unwrap();
        let secret = |byte: u8| EndpointSecret {
            salt: vec![byte; 32],
            token_hash: vec![byte; 32],
        };
        for (node, byte) in [("a", 1), ("b", 2)] {
            webhooks
                .ensure("alice", "wf-1", node, secret(byte))
                .await
                .unwrap();
        }

        // Mise à jour sans le nœud `b` : seule son URL disparaît.
        wf["nodes"].as_array_mut().unwrap().pop();
        let r = app
            .clone()
            .oneshot(auth_req("PUT", "/wf-1", &token, Some(&wf.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(webhooks.resolve(&[1; 32]).await.unwrap().is_some());
        assert!(webhooks.resolve(&[2; 32]).await.unwrap().is_none());

        // Suppression du workflow : plus aucune URL.
        let r = app
            .oneshot(auth_req("DELETE", "/wf-1", &token, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(webhooks.resolve(&[1; 32]).await.unwrap().is_none());
    }

    // ───── #227 — déclencheurs planifiés ─────

    /// Store d'échéances qui échoue sur commande (base indisponible).
    #[derive(Default)]
    struct FlakySchedules {
        inner: autome_core::InMemoryTriggerScheduleStore,
        fail: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl TriggerScheduleStore for FlakySchedules {
        async fn sync(
            &self,
            owner: &str,
            workflow_id: &str,
            triggers: &[ScheduledTrigger],
        ) -> autome_core::Result<()> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(autome_core::Error::Invalid("base indisponible".into()));
            }
            self.inner.sync(owner, workflow_id, triggers).await
        }
        async fn due(
            &self,
            now_ms: i64,
            limit: usize,
        ) -> autome_core::Result<Vec<autome_core::ScheduleSlot>> {
            self.inner.due(now_ms, limit).await
        }
        async fn advance(
            &self,
            slot: &autome_core::ScheduleSlot,
            update: autome_core::SlotUpdate,
        ) -> autome_core::Result<bool> {
            self.inner.advance(slot, update).await
        }
    }

    fn with_trigger(id: &str, trigger_type: &str, params: serde_json::Value) -> serde_json::Value {
        let mut wf = sample_workflow_json(id);
        wf["nodes"] = serde_json::json!([{
            "id": "entry", "name": "entry", "kind": "trigger",
            "trigger_type": trigger_type, "params": params
        }]);
        wf
    }

    async fn scheduled(store: &FlakySchedules) -> Vec<(String, String, serde_json::Value)> {
        store
            .due(i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.workflow_id, s.node_id, s.params))
            .collect()
    }

    #[tokio::test]
    async fn invalid_trigger_parameters_are_rejected_with_400() {
        let (state, svc, perms, _, repo) = make_test_state();
        let app = workflows_router(state);
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();

        let bad = with_trigger(
            "wf-1",
            "schedule",
            serde_json::json!({ "cron": "99 * * * *" }),
        );
        let r = app
            .clone()
            .oneshot(auth_req("POST", "/", &token, Some(&bad.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let msg = body_string(r).await;
        assert!(
            msg.contains("nœud `entry` (schedule)") && msg.contains("hors bornes"),
            "{msg}"
        );
        assert!(repo
            .get("alice", &WorkflowId::new("wf-1"))
            .await
            .unwrap()
            .is_none());

        // Mise à jour : même contrôle, workflow inchangé.
        repo.create("alice", sample_workflow("wf-2")).await.unwrap();
        let bad = with_trigger("wf-2", "polling", serde_json::json!({ "url": "ftp://x" }));
        let r = app
            .oneshot(auth_req("PUT", "/wf-2", &token, Some(&bad.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            repo.get("alice", &WorkflowId::new("wf-2")).await.unwrap(),
            Some(sample_workflow("wf-2"))
        );
    }

    #[tokio::test]
    async fn schedules_follow_create_update_and_delete() {
        let (state, svc, perms, _, _) = make_test_state();
        let store = Arc::new(FlakySchedules::default());
        let app = workflows_router(state.with_schedules(store.clone()));
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();

        let cron = serde_json::json!({ "cron": "0 9 * * 1-5" });
        let wf = with_trigger("wf-1", "schedule", cron.clone());
        let r = app
            .clone()
            .oneshot(auth_req("POST", "/", &token, Some(&wf.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(
            scheduled(&store).await,
            vec![("wf-1".into(), "entry".into(), cron)]
        );
        assert_eq!(store.due(i64::MAX, 1).await.unwrap()[0].owner_id, "alice");

        // Devient un webhook : plus d'échéance.
        let hook = with_trigger("wf-1", "webhook", serde_json::json!({}));
        let r = app
            .clone()
            .oneshot(auth_req("PUT", "/wf-1", &token, Some(&hook.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(scheduled(&store).await.is_empty());

        // Polling, puis suppression du workflow.
        let poll = serde_json::json!({ "url": "https://api.example/items" });
        let wf = with_trigger("wf-1", "polling", poll.clone());
        app.clone()
            .oneshot(auth_req("PUT", "/wf-1", &token, Some(&wf.to_string())))
            .await
            .unwrap();
        assert_eq!(
            scheduled(&store).await,
            vec![("wf-1".into(), "entry".into(), poll)]
        );
        let r = app
            .oneshot(auth_req("DELETE", "/wf-1", &token, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(scheduled(&store).await.is_empty());
    }

    #[tokio::test]
    async fn failed_schedule_sync_undoes_a_create_so_it_can_be_retried() {
        let (state, svc, perms, _, repo) = make_test_state();
        let store = Arc::new(FlakySchedules::default());
        let app = workflows_router(state.with_schedules(store.clone()));
        perms.assign_role("alice", &Role::editor()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let wf = with_trigger(
            "wf-1",
            "schedule",
            serde_json::json!({ "interval_seconds": 60 }),
        );

        store.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let r = app
            .clone()
            .oneshot(auth_req("POST", "/", &token, Some(&wf.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(repo
            .get("alice", &WorkflowId::new("wf-1"))
            .await
            .unwrap()
            .is_none());

        // Nouvel essai : pas de 409.
        store.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        let r = app
            .clone()
            .oneshot(auth_req("POST", "/", &token, Some(&wf.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        let changed = with_trigger(
            "wf-1",
            "schedule",
            serde_json::json!({ "interval_seconds": 120 }),
        );
        // Mise à jour qui échoue : 500, à renvoyer telle quelle.
        store.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let r = app
            .clone()
            .oneshot(auth_req("PUT", "/wf-1", &token, Some(&changed.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        store.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        let r = app
            .oneshot(auth_req("PUT", "/wf-1", &token, Some(&changed.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            scheduled(&store).await[0].2,
            serde_json::json!({ "interval_seconds": 120 })
        );
    }
}

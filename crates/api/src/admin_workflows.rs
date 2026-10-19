//! Liste d'administration de tous les workflows (#243).
//!
//! | Méthode | Path                                         | Permission | Succès |
//! |---------|----------------------------------------------|------------|--------|
//! | GET     | `/admin/workflows?owner=&q=&offset=&limit=`  | `*`        | 200 + [`AdminWorkflowListResponse`] |
//!
//! Depuis le namespacing des ids (#188), `GET /workflows` ne liste que
//! l'espace de l'appelant et un super-admin n'atteint celui d'un autre que
//! compte par compte (`?owner=`). Cette route donne la vue d'ensemble :
//! workflows de **tous** les espaces, avec leur propriétaire.
//!
//! Règles :
//! - réservée au **super-admin** (`*`), comme `?owner=` qui est la seule façon
//!   d'agir ensuite sur ces workflows : `users.admin` seul ne suffit pas
//!   (403) ;
//! - **résumé, pas le graphe** : propriétaire, id, nom, nombre de nœuds. Les
//!   paramètres des nœuds restent lisibles par
//!   `GET /workflows/{id}?owner=<owner_id>` ;
//! - tri stable par `(owner_id, id)` ; `owner` filtre un espace exact (vide :
//!   workflows sans propriétaire, antérieurs à la migration `0010`), `q`
//!   cherche une sous-chaîne de l'id ou du nom, sans tenir compte de la casse ;
//! - emails des propriétaires lus en **une** requête par page ; `owner_email`
//!   est `null` pour l'espace legacy ou un compte qui n'existe plus ;
//! - chaque appel produit un event d'audit `admin.workflows_list`.

use std::collections::BTreeSet;
use std::sync::Arc;

use autome_core::{
    AuditEvent, AuditOutcome, Pagination, UserStore, WorkflowFilter, WorkflowRepository,
    PERM_WILDCARD,
};
use axum::{
    extract::{FromRef, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::{IntoParams, ToSchema};

use crate::admin::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};
use crate::auth::{AuthError, AuthState, AuthenticatedUser};

/// État du handler.
#[derive(Clone)]
pub struct AdminWorkflowsState {
    /// État d'authentification (permissions, audit, horloge).
    pub auth: AuthState,
    /// Workflows de tous les espaces.
    pub workflows: Arc<dyn WorkflowRepository + Send + Sync>,
    /// Comptes, pour l'email des propriétaires.
    pub users: Arc<dyn UserStore + Send + Sync>,
}

impl FromRef<AdminWorkflowsState> for AuthState {
    fn from_ref(input: &AdminWorkflowsState) -> Self {
        input.auth.clone()
    }
}

/// Paramètres de la liste (`?owner=&q=&offset=0&limit=50`).
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct AdminWorkflowListQuery {
    /// Espace exact (id du propriétaire ; vide = workflows sans
    /// propriétaire). Absent = tous les espaces.
    #[serde(default)]
    pub owner: Option<String>,
    /// Sous-chaîne de l'id ou du nom (insensible à la casse).
    #[serde(default)]
    pub q: Option<String>,
    /// Offset, défaut 0.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Limite, défaut 50, maximum 200.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Workflow vu par l'administration.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AdminWorkflow {
    /// Propriétaire (espace d'ids) ; vide = sans propriétaire (legacy).
    pub owner_id: String,
    /// Email du propriétaire ; `null` sans compte correspondant.
    pub owner_email: Option<String>,
    /// Id dans l'espace du propriétaire.
    pub id: String,
    /// Nom affiché.
    pub name: String,
    /// Nombre de nœuds de la version courante.
    pub node_count: usize,
}

/// Page de [`list_workflows`].
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AdminWorkflowListResponse {
    /// Workflows de la page, triés par propriétaire puis par id.
    pub items: Vec<AdminWorkflow>,
    /// Total des workflows correspondant au filtre.
    pub total: usize,
    /// Offset appliqué.
    pub offset: usize,
    /// Limite appliquée.
    pub limit: usize,
}

/// Erreurs du handler.
#[derive(Debug)]
pub enum AdminWorkflowsError {
    /// Authentification ou permission refusée (401/403).
    Auth(AuthError),
    /// Erreur interne (store).
    Internal(String),
}

impl From<AuthError> for AdminWorkflowsError {
    fn from(e: AuthError) -> Self {
        Self::Auth(e)
    }
}

impl IntoResponse for AdminWorkflowsError {
    fn into_response(self) -> Response {
        match self {
            Self::Auth(e) => e.into_response(),
            Self::Internal(msg) => {
                tracing::error!(error = %msg, "erreur interne de la liste des workflows");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

fn internal(e: impl std::fmt::Display) -> AdminWorkflowsError {
    AdminWorkflowsError::Internal(e.to_string())
}

/// `GET /admin/workflows` — workflows de tous les espaces, paginés.
#[utoipa::path(
    get,
    path = "/admin/workflows",
    tag = "admin",
    params(AdminWorkflowListQuery),
    responses(
        (status = 200, description = "Page de workflows, tous propriétaires confondus", body = AdminWorkflowListResponse),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Réservé au super-admin (`*`)")
    ),
    security(("bearer" = []))
)]
pub async fn list_workflows(
    user: AuthenticatedUser,
    State(state): State<AdminWorkflowsState>,
    Query(q): Query<AdminWorkflowListQuery>,
) -> Result<Json<AdminWorkflowListResponse>, AdminWorkflowsError> {
    state.auth.require_permission(&user, PERM_WILDCARD).await?;
    let filter = WorkflowFilter {
        owner: q.owner,
        query: q.q,
    };
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE);
    let page = state
        .workflows
        .list_all(&filter, Pagination { offset, limit })
        .await
        .map_err(internal)?;
    let owners: Vec<String> = page
        .items
        .iter()
        .map(|w| w.owner.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let emails = state.users.emails(&owners).await.map_err(internal)?;
    audit(&state, user.user_id(), &filter).await;
    let items = page
        .items
        .into_iter()
        .map(|w| AdminWorkflow {
            owner_email: emails.get(&w.owner).cloned(),
            owner_id: w.owner,
            id: w.id.0,
            name: w.name,
            node_count: w.node_count,
        })
        .collect();
    Ok(Json(AdminWorkflowListResponse {
        items,
        total: page.total,
        offset,
        limit,
    }))
}

async fn audit(state: &AdminWorkflowsState, actor: &str, filter: &WorkflowFilter) {
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let metadata = match &filter.owner {
        Some(owner) => json!({ "owner": owner }),
        None => Value::Null,
    };
    let event = AuditEvent::new(now_ms, actor, "admin.workflows_list", AuditOutcome::Success)
        .with_resource("*")
        .with_metadata(metadata);
    if let Err(e) = state.auth.audit.record(event).await {
        tracing::error!(error = %e, "événement d'audit non enregistré");
    }
}

/// Route `/admin/workflows` (à fusionner à plat dans le router racine).
pub fn admin_workflows_router(state: AdminWorkflowsState) -> Router {
    Router::new()
        .route("/admin/workflows", get(list_workflows))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{
        AuditSink, CreateUser, InMemoryAuditSink, InMemoryPermissionStore, InMemoryRevocationStore,
        InMemoryUserStore, InMemoryWorkflowRepository, Node, NodeId, NodeKind, PermissionStore,
        RevocationStore, Role, TokenService, WorkflowGraph, WorkflowId, LEGACY_NAMESPACE,
        PERM_USERS_ADMIN, PERM_WORKFLOWS_READ,
    };
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const NOW: i64 = 1_700_000_000;

    struct FixedClock;
    impl crate::auth::Clock for FixedClock {
        fn now_unix_seconds(&self) -> i64 {
            NOW
        }
    }

    struct Fixture {
        app: Router,
        users: Arc<InMemoryUserStore>,
        workflows: Arc<InMemoryWorkflowRepository>,
        audit: Arc<InMemoryAuditSink>,
    }

    impl Fixture {
        fn new() -> Self {
            let perms = Arc::new(InMemoryPermissionStore::new());
            let audit = Arc::new(InMemoryAuditSink::new());
            let users = Arc::new(InMemoryUserStore::new(Arc::clone(&perms)));
            let workflows = Arc::new(InMemoryWorkflowRepository::new());
            let auth = AuthState::new(
                TokenService::new(SECRET),
                Arc::new(InMemoryRevocationStore::new()) as Arc<dyn RevocationStore + Send + Sync>,
                perms as Arc<dyn PermissionStore + Send + Sync>,
                Arc::clone(&audit) as Arc<dyn AuditSink + Send + Sync>,
            )
            .with_clock(Arc::new(FixedClock));
            let app = admin_workflows_router(AdminWorkflowsState {
                auth,
                workflows: workflows.clone(),
                users: users.clone(),
            });
            Self {
                app,
                users,
                workflows,
                audit,
            }
        }

        async fn account(&self, email: &str, role: Role) -> String {
            match self
                .users
                .create(email, "$argon2id$h", &role.permissions, true)
                .await
                .unwrap()
            {
                CreateUser::Created(u) => u.id,
                CreateUser::EmailTaken => panic!("email pris"),
            }
        }

        async fn workflow(&self, owner: &str, id: &str, name: &str, nodes: usize) {
            let node = |i: usize| Node {
                position: None,
                id: NodeId::new(format!("n{i}")),
                name: format!("n{i}"),
                kind: NodeKind::Action {
                    action_type: "log".into(),
                },
                params: Value::Null,
            };
            let graph = WorkflowGraph {
                id: WorkflowId::new(id),
                name: name.into(),
                nodes: (0..nodes).map(node).collect(),
                edges: vec![],
            };
            self.workflows.create(owner, graph).await.unwrap();
        }

        async fn get(&self, uri: &str, as_user: &str) -> Response {
            let token = TokenService::new(SECRET)
                .issue_pair(as_user, 0, 900, 3_600, NOW)
                .unwrap()
                .0;
            let req = Request::builder()
                .uri(uri)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            self.app.clone().oneshot(req).await.unwrap()
        }
    }

    async fn json_body(response: Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// `(owner_email, id)` de chaque ligne de la page.
    fn rows(page: &Value) -> Vec<(Option<String>, String)> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["owner_email"].as_str().map(String::from),
                    w["id"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn row(email: Option<&str>, id: &str) -> (Option<String>, String) {
        (email.map(String::from), id.to_string())
    }

    #[tokio::test]
    async fn reserved_to_super_admins_even_for_users_admin() {
        let fx = Fixture::new();
        let editor = fx.account("ed@example.io", Role::editor()).await;
        let user_admin = fx
            .account(
                "ua@example.io",
                Role::new("user-admin").with_permissions([PERM_USERS_ADMIN, PERM_WORKFLOWS_READ]),
            )
            .await;
        for caller in [&editor, &user_admin] {
            let res = fx.get("/admin/workflows", caller).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{caller}");
        }
        let anonymous = Request::builder()
            .uri("/admin/workflows")
            .body(Body::empty())
            .unwrap();
        let res = fx.app.clone().oneshot(anonymous).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let listed = fx
            .audit
            .list()
            .await
            .unwrap()
            .into_iter()
            .any(|e| e.action == "admin.workflows_list");
        assert!(!listed, "aucune liste servie");
    }

    #[tokio::test]
    async fn lists_every_namespace_with_owner_emails() {
        let fx = Fixture::new();
        let root = fx.account("root@example.io", Role::super_admin()).await;
        let alice = fx.account("alice@example.io", Role::editor()).await;
        let bob = fx.account("bob@example.io", Role::editor()).await;
        fx.workflow(&alice, "wf-draft", "Brouillon d'alice", 3)
            .await;
        fx.workflow(&bob, "wf-draft", "Brouillon de bob", 1).await;
        fx.workflow(LEGACY_NAMESPACE, "legacy", "Ancien", 2).await;
        fx.workflow("compte-disparu", "orphan", "Orphelin", 0).await;

        let res = fx.get("/admin/workflows", &root).await;
        assert_eq!(res.status(), StatusCode::OK);
        let page = json_body(res).await;
        assert_eq!(page["total"], 4);
        assert_eq!(
            (page["offset"].clone(), page["limit"].clone()),
            (json!(0), json!(50))
        );
        let items = page["items"].as_array().unwrap();
        // Tri par propriétaire puis id : legacy (""), puis les ids d'alice et
        // bob (UUID aléatoires), puis « compte-disparu ».
        assert_eq!(items[0]["owner_id"], "");
        assert_eq!(items[0]["owner_email"], Value::Null);
        assert_eq!(items[0]["node_count"], 2);
        let alice_row = items
            .iter()
            .find(|w| w["owner_id"] == alice.as_str())
            .unwrap();
        assert_eq!(
            alice_row,
            &json!({
                "owner_id": alice,
                "owner_email": "alice@example.io",
                "id": "wf-draft",
                "name": "Brouillon d'alice",
                "node_count": 3,
            })
        );
        let bob_row = items
            .iter()
            .find(|w| w["owner_id"] == bob.as_str())
            .unwrap();
        assert_eq!(bob_row["owner_email"], "bob@example.io");
        let orphan = items.iter().find(|w| w["id"] == "orphan").unwrap();
        assert_eq!(orphan["owner_email"], Value::Null, "compte inexistant");
        assert!(
            items.iter().all(|w| w.get("nodes").is_none()),
            "jamais le graphe"
        );

        let events = fx.audit.list().await.unwrap();
        let listed = events
            .iter()
            .find(|e| e.action == "admin.workflows_list")
            .unwrap();
        assert_eq!(listed.actor, root);
        assert_eq!(listed.outcome, AuditOutcome::Success);
    }

    #[tokio::test]
    async fn filters_by_owner_and_search_then_paginates() {
        let fx = Fixture::new();
        let root = fx.account("root@example.io", Role::super_admin()).await;
        let alice = fx.account("alice@example.io", Role::editor()).await;
        fx.workflow(&alice, "wf-a", "Factures", 1).await;
        fx.workflow(&alice, "wf-b", "Relances", 1).await;
        fx.workflow(&root, "wf-c", "Factures du super-admin", 1)
            .await;
        fx.workflow(LEGACY_NAMESPACE, "old", "Ancien", 1).await;

        let alice_only = json_body(
            fx.get(&format!("/admin/workflows?owner={alice}"), &root)
                .await,
        )
        .await;
        assert_eq!(
            rows(&alice_only),
            vec![
                row(Some("alice@example.io"), "wf-a"),
                row(Some("alice@example.io"), "wf-b")
            ]
        );
        let legacy = json_body(fx.get("/admin/workflows?owner=", &root).await).await;
        assert_eq!(rows(&legacy), vec![row(None, "old")]);

        let search = json_body(fx.get("/admin/workflows?q=FACTURES", &root).await).await;
        assert_eq!(search["total"], 2);
        let both = json_body(
            fx.get(&format!("/admin/workflows?owner={alice}&q=relan"), &root)
                .await,
        )
        .await;
        assert_eq!(rows(&both), vec![row(Some("alice@example.io"), "wf-b")]);

        let paged = json_body(fx.get("/admin/workflows?offset=1&limit=2", &root).await).await;
        assert_eq!(paged["total"], 4);
        assert_eq!(paged["items"].as_array().unwrap().len(), 2);
        assert_eq!(
            (paged["offset"].clone(), paged["limit"].clone()),
            (json!(1), json!(2))
        );
        let capped = json_body(fx.get("/admin/workflows?limit=100000", &root).await).await;
        assert_eq!(capped["limit"], MAX_PAGE_SIZE);

        let audited = fx
            .audit
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "admin.workflows_list")
            .map(|e| e.metadata)
            .collect::<Vec<_>>();
        assert_eq!(audited[0], json!({ "owner": alice }));
        assert_eq!(audited[1], json!({ "owner": "" }));
    }
}

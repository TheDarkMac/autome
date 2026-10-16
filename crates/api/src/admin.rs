//! Administration des utilisateurs (#208).
//!
//! Endpoints REST/JSON, tous protégés par la permission `users.admin` :
//!
//! | Méthode | Path                             | Body         | Succès               |
//! |---------|----------------------------------|--------------|----------------------|
//! | GET     | `/admin/users?q=&offset=&limit=` | —            | 200 + [`UserListResponse`] |
//! | GET     | `/admin/users/{id}`              | —            | 200 + [`AdminUser`]  |
//! | PUT     | `/admin/users/{id}/role`         | `{ role }`   | 200 + [`AdminUser`]  |
//! | POST    | `/admin/users/{id}/disable`      | —            | 200 + [`AdminUser`]  |
//! | POST    | `/admin/users/{id}/enable`       | —            | 200 + [`AdminUser`]  |
//! | POST    | `/admin/users/{id}/logout-all`   | —            | 204                  |
//!
//! Règles :
//! - le **rôle** (`admin`, `editor`, `viewer`) n'est qu'une vue des
//!   permissions : en changer remplace toutes les permissions du compte, en
//!   une transaction ; l'effet est immédiat (les permissions sont relues à
//!   chaque requête). Un jeu qui ne correspond à aucun rôle s'affiche
//!   `custom` ;
//! - **pas d'escalade** : attribuer le rôle `admin` (wildcard `*`) ou agir sur
//!   un compte qui le détient exige soi-même `*` — `users.admin` seul ne
//!   suffit pas (403) ;
//! - **pas de verrouillage** : un administrateur ne change pas son propre rôle
//!   et ne se suspend pas lui-même (409). Le premier administrateur se crée
//!   hors API (`autome-api promote-admin <email>`) ;
//! - la **suspension** est réversible : le compte garde ses données et ses
//!   permissions, sa connexion est refusée et **toutes** ses sessions sont
//!   fermées (version de session, #186). Le drapeau est écrit *avant*
//!   l'incrément de version, et la connexion relit le drapeau *après* avoir
//!   lu la version (cf. [`crate::accounts`]) : une connexion concurrente de la
//!   suspension est soit refusée, soit émise à une version aussitôt périmée ;
//! - compte inconnu : 404 ; chaque appel produit un event d'audit
//!   `admin.users_list` / `admin.user_get` / `admin.user_role` /
//!   `admin.user_disable` / `admin.user_enable` / `admin.user_logout_all`
//!   (ressource = compte visé).

use std::collections::HashSet;
use std::sync::Arc;

use autome_core::{
    normalize_email, role_of, AuditEvent, AuditOutcome, AuditSink, Role, UserStore, UserSummary,
    PERM_USERS_ADMIN, PERM_WILDCARD, ROLE_ADMIN,
};
use axum::{
    extract::{FromRef, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::{IntoParams, ToSchema};

use crate::auth::{AuthError, AuthState, AuthenticatedUser, OwnerScope};

/// Taille de page par défaut de la liste.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// Taille de page maximale de la liste.
pub const MAX_PAGE_SIZE: usize = 200;

/// État des handlers d'administration.
#[derive(Clone)]
pub struct AdminState {
    /// État d'authentification (permissions, révocations, audit, horloge).
    pub auth: AuthState,
    /// Comptes utilisateurs.
    pub users: Arc<dyn UserStore + Send + Sync>,
}

impl AdminState {
    /// Construit l'état.
    pub fn new(auth: AuthState, users: Arc<dyn UserStore + Send + Sync>) -> Self {
        Self { auth, users }
    }
}

impl FromRef<AdminState> for AuthState {
    fn from_ref(input: &AdminState) -> Self {
        input.auth.clone()
    }
}

/// Paramètres de la liste (`?q=&offset=0&limit=50`).
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct UserListQuery {
    /// Filtre : sous-chaîne de l'email (insensible à la casse).
    #[serde(default)]
    pub q: Option<String>,
    /// Offset, défaut 0.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Limite, défaut 50, maximum 200.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Compte vu par l'administration — jamais le hash du mot de passe.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AdminUser {
    /// Identifiant (`claims.sub`).
    pub id: String,
    /// Email normalisé.
    pub email: String,
    /// Rôle déduit des permissions : `admin`, `editor`, `viewer` ou `custom`.
    pub role: String,
    /// Permissions accordées, triées.
    pub permissions: Vec<String>,
    /// Compte suspendu.
    pub disabled: bool,
    /// Création du compte (millisecondes Unix).
    pub created_at_ms: i64,
}

impl From<UserSummary> for AdminUser {
    fn from(user: UserSummary) -> Self {
        let mut permissions: Vec<String> = user.permissions.iter().cloned().collect();
        permissions.sort();
        Self {
            role: role_of(&user.permissions).to_string(),
            id: user.id,
            email: user.email,
            permissions,
            disabled: user.disabled,
            created_at_ms: user.created_at_ms,
        }
    }
}

/// Page de [`list_users`].
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserListResponse {
    /// Comptes de la page, du plus ancien au plus récent.
    pub items: Vec<AdminUser>,
    /// Total des comptes correspondant au filtre.
    pub total: usize,
    /// Offset appliqué.
    pub offset: usize,
    /// Limite appliquée.
    pub limit: usize,
}

/// Corps de [`set_role`].
#[derive(Debug, Deserialize, ToSchema)]
pub struct RoleBody {
    /// `admin`, `editor` ou `viewer`.
    pub role: String,
}

/// Erreurs des handlers d'administration.
#[derive(Debug)]
pub enum AdminError {
    /// Authentification ou permission refusée (401/403).
    Auth(AuthError),
    /// Compte inconnu.
    NotFound,
    /// Saisie invalide (rôle inconnu).
    BadRequest(String),
    /// Action refusée sur son propre compte.
    Conflict(&'static str),
    /// Erreur interne (store).
    Internal(String),
}

impl From<AuthError> for AdminError {
    fn from(e: AuthError) -> Self {
        Self::Auth(e)
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        match self {
            AdminError::Auth(e) => e.into_response(),
            AdminError::NotFound => (StatusCode::NOT_FOUND, "user not found").into_response(),
            AdminError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            AdminError::Conflict(msg) => (StatusCode::CONFLICT, msg).into_response(),
            AdminError::Internal(msg) => {
                tracing::error!(error = %msg, "erreur interne de l'administration");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

fn internal(e: impl std::fmt::Display) -> AdminError {
    AdminError::Internal(e.to_string())
}

async fn audit(
    state: &AdminState,
    actor: &str,
    action: &str,
    target: &str,
    outcome: AuditOutcome,
    metadata: Value,
) {
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let event = AuditEvent::new(now_ms, actor, action, outcome)
        .with_resource(target)
        .with_metadata(metadata);
    if let Err(e) = state.auth.audit.record(event).await {
        tracing::error!(action, error = %e, "événement d'audit non enregistré");
    }
}

/// Compte visé par une action, après vérification que `caller` peut y
/// toucher : 404 s'il est inconnu, 403 s'il détient `*` et pas l'appelant.
async fn target_for_change(
    state: &AdminState,
    caller: &AuthenticatedUser,
    id: &str,
    action: &str,
) -> Result<UserSummary, AdminError> {
    let Some(target) = state.users.summary(id).await.map_err(internal)? else {
        return Err(AdminError::NotFound);
    };
    if target.permissions.contains(PERM_WILDCARD) {
        require_wildcard(state, caller, id, action).await?;
    }
    Ok(target)
}

/// Exige le wildcard `*` de l'appelant (403 audité sinon).
async fn require_wildcard(
    state: &AdminState,
    caller: &AuthenticatedUser,
    target: &str,
    action: &str,
) -> Result<(), AdminError> {
    if state.auth.owner_scope(caller).await? == OwnerScope::All {
        return Ok(());
    }
    audit(
        state,
        caller.user_id(),
        action,
        target,
        AuditOutcome::Denied,
        json!({ "reason": "super-admin required" }),
    )
    .await;
    Err(AuthError::Forbidden(PERM_WILDCARD.to_string()).into())
}

/// Refus d'une action sur son propre compte (409 audité).
async fn refuse_self(
    state: &AdminState,
    caller: &AuthenticatedUser,
    action: &str,
    message: &'static str,
) -> AdminError {
    audit(
        state,
        caller.user_id(),
        action,
        caller.user_id(),
        AuditOutcome::Denied,
        json!({ "reason": "own account" }),
    )
    .await;
    AdminError::Conflict(message)
}

/// Relit le compte après une écriture (vue à jour pour la réponse).
async fn reload(state: &AdminState, id: &str) -> Result<AdminUser, AdminError> {
    match state.users.summary(id).await.map_err(internal)? {
        Some(user) => Ok(user.into()),
        // Compte disparu entre l'écriture et la relecture.
        None => Err(AdminError::NotFound),
    }
}

/// `GET /admin/users` — comptes paginés, du plus ancien au plus récent.
#[utoipa::path(
    get,
    path = "/admin/users",
    tag = "admin",
    params(UserListQuery),
    responses(
        (status = 200, description = "Page de comptes", body = UserListResponse),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`")
    ),
    security(("bearer" = []))
)]
pub async fn list_users(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Query(q): Query<UserListQuery>,
) -> Result<Json<UserListResponse>, AdminError> {
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    let query =
        q.q.as_deref()
            .map(normalize_email)
            .filter(|q| !q.is_empty());
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE);
    let page = state
        .users
        .list(query.as_deref(), offset, limit)
        .await
        .map_err(internal)?;
    audit(
        &state,
        user.user_id(),
        "admin.users_list",
        "*",
        AuditOutcome::Success,
        Value::Null,
    )
    .await;
    Ok(Json(UserListResponse {
        items: page.items.into_iter().map(AdminUser::from).collect(),
        total: page.total,
        offset,
        limit,
    }))
}

/// `GET /admin/users/{id}` — un compte et ses permissions.
#[utoipa::path(
    get,
    path = "/admin/users/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Identifiant du compte")),
    responses(
        (status = 200, description = "Compte", body = AdminUser),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`"),
        (status = 404, description = "Compte inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn get_user(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Json<AdminUser>, AdminError> {
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    let found = state.users.summary(&id).await.map_err(internal)?;
    audit(
        &state,
        user.user_id(),
        "admin.user_get",
        &id,
        AuditOutcome::Success,
        Value::Null,
    )
    .await;
    found.map(|u| Json(u.into())).ok_or(AdminError::NotFound)
}

/// `PUT /admin/users/{id}/role` — remplace les permissions du compte par
/// celles du rôle.
#[utoipa::path(
    put,
    path = "/admin/users/{id}/role",
    tag = "admin",
    params(("id" = String, Path, description = "Identifiant du compte")),
    request_body = RoleBody,
    responses(
        (status = 200, description = "Rôle attribué", body = AdminUser),
        (status = 400, description = "Rôle inconnu"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`, ou `*` pour le rôle `admin` ou un compte super-admin"),
        (status = 404, description = "Compte inconnu"),
        (status = 409, description = "Son propre rôle")
    ),
    security(("bearer" = []))
)]
pub async fn set_role(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<RoleBody>,
) -> Result<Json<AdminUser>, AdminError> {
    const ACTION: &str = "admin.user_role";
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    let Some(role) = Role::standard(&body.role) else {
        return Err(AdminError::BadRequest(
            "unknown role (expected admin, editor or viewer)".into(),
        ));
    };
    if id == user.user_id() {
        return Err(refuse_self(&state, &user, ACTION, "cannot change your own role").await);
    }
    let target = target_for_change(&state, &user, &id, ACTION).await?;
    if role.name == ROLE_ADMIN {
        require_wildcard(&state, &user, &id, ACTION).await?;
    }
    let previous = role_of(&target.permissions);
    let permissions: HashSet<String> = role.permissions;
    if !state
        .users
        .set_permissions(&id, &permissions)
        .await
        .map_err(internal)?
    {
        return Err(AdminError::NotFound);
    }
    tracing::info!(target_id = %id, role = %role.name, "rôle attribué");
    audit(
        &state,
        user.user_id(),
        ACTION,
        &id,
        AuditOutcome::Success,
        json!({ "from": previous, "to": role.name }),
    )
    .await;
    Ok(Json(reload(&state, &id).await?))
}

/// `POST /admin/users/{id}/disable` — suspend le compte et ferme toutes ses
/// sessions.
#[utoipa::path(
    post,
    path = "/admin/users/{id}/disable",
    tag = "admin",
    params(("id" = String, Path, description = "Identifiant du compte")),
    responses(
        (status = 200, description = "Compte suspendu, sessions fermées", body = AdminUser),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`, ou `*` pour un compte super-admin"),
        (status = 404, description = "Compte inconnu"),
        (status = 409, description = "Son propre compte")
    ),
    security(("bearer" = []))
)]
pub async fn disable_user(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Json<AdminUser>, AdminError> {
    const ACTION: &str = "admin.user_disable";
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    if id == user.user_id() {
        return Err(refuse_self(&state, &user, ACTION, "cannot disable your own account").await);
    }
    target_for_change(&state, &user, &id, ACTION).await?;
    // Drapeau d'abord, version ensuite : cf. la doc du module.
    if !state
        .users
        .set_disabled(&id, true)
        .await
        .map_err(internal)?
    {
        return Err(AdminError::NotFound);
    }
    let version = state
        .auth
        .revocation
        .bump_session_version(&id)
        .await
        .map_err(internal)?;
    tracing::info!(target_id = %id, session_version = version, "compte suspendu, sessions fermées");
    audit(
        &state,
        user.user_id(),
        ACTION,
        &id,
        AuditOutcome::Success,
        Value::Null,
    )
    .await;
    Ok(Json(reload(&state, &id).await?))
}

/// `POST /admin/users/{id}/enable` — réactive un compte suspendu (ses
/// anciennes sessions restent fermées).
#[utoipa::path(
    post,
    path = "/admin/users/{id}/enable",
    tag = "admin",
    params(("id" = String, Path, description = "Identifiant du compte")),
    responses(
        (status = 200, description = "Compte réactivé", body = AdminUser),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`, ou `*` pour un compte super-admin"),
        (status = 404, description = "Compte inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn enable_user(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Json<AdminUser>, AdminError> {
    const ACTION: &str = "admin.user_enable";
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    target_for_change(&state, &user, &id, ACTION).await?;
    if !state
        .users
        .set_disabled(&id, false)
        .await
        .map_err(internal)?
    {
        return Err(AdminError::NotFound);
    }
    tracing::info!(target_id = %id, "compte réactivé");
    audit(
        &state,
        user.user_id(),
        ACTION,
        &id,
        AuditOutcome::Success,
        Value::Null,
    )
    .await;
    Ok(Json(reload(&state, &id).await?))
}

/// `POST /admin/users/{id}/logout-all` — ferme toutes les sessions du
/// compte, sur tous ses appareils.
#[utoipa::path(
    post,
    path = "/admin/users/{id}/logout-all",
    tag = "admin",
    params(("id" = String, Path, description = "Identifiant du compte")),
    responses(
        (status = 204, description = "Sessions fermées"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`, ou `*` pour un compte super-admin"),
        (status = 404, description = "Compte inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn logout_user(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AdminError> {
    const ACTION: &str = "admin.user_logout_all";
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    target_for_change(&state, &user, &id, ACTION).await?;
    let version = state
        .auth
        .revocation
        .bump_session_version(&id)
        .await
        .map_err(internal)?;
    tracing::info!(target_id = %id, session_version = version, "sessions fermées par un administrateur");
    audit(
        &state,
        user.user_id(),
        ACTION,
        &id,
        AuditOutcome::Success,
        Value::Null,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Issue de [`promote_admin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Promotion {
    /// Compte promu super-admin (`reactivated` : il était suspendu).
    Promoted {
        /// Identifiant du compte.
        user_id: String,
        /// Le compte était suspendu et a été réactivé.
        reactivated: bool,
    },
    /// Aucun compte pour cet email.
    UnknownEmail,
}

/// Amorçage hors API (`autome-api promote-admin <email>`) : donne le rôle
/// `admin` (wildcard `*`) au compte de `email` et le réactive s'il était
/// suspendu — outil de création du premier administrateur et de
/// récupération après un verrouillage. Il exige un accès à l'infrastructure
/// (base de données) : jamais exposé par HTTP. Audité `admin.user_promote`
/// sous l'acteur `cli`.
pub async fn promote_admin(
    users: &(dyn UserStore + Send + Sync),
    audit: &(dyn AuditSink + Send + Sync),
    email: &str,
    now_ms: i64,
) -> autome_core::Result<Promotion> {
    let email = normalize_email(email);
    let Some(account) = users.find_by_email(&email).await? else {
        return Ok(Promotion::UnknownEmail);
    };
    if !users
        .set_permissions(&account.id, &Role::super_admin().permissions)
        .await?
    {
        return Ok(Promotion::UnknownEmail);
    }
    if account.disabled {
        users.set_disabled(&account.id, false).await?;
    }
    let event = AuditEvent::new(now_ms, "cli", "admin.user_promote", AuditOutcome::Success)
        .with_resource(&account.id)
        .with_metadata(json!({ "reactivated": account.disabled }));
    if let Err(e) = audit.record(event).await {
        tracing::error!(error = %e, "événement d'audit non enregistré");
    }
    Ok(Promotion::Promoted {
        user_id: account.id,
        reactivated: account.disabled,
    })
}

/// Routes `/admin/users…` (à fusionner à plat dans le router racine).
pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/users", get(list_users))
        .route("/admin/users/:id", get(get_user))
        .route("/admin/users/:id/role", put(set_role))
        .route("/admin/users/:id/disable", post(disable_user))
        .route("/admin/users/:id/enable", post(enable_user))
        .route("/admin/users/:id/logout-all", post(logout_user))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{
        AuditSink, CreateUser, InMemoryAuditSink, InMemoryPermissionStore, InMemoryRevocationStore,
        InMemoryUserStore, PermissionStore, RevocationStore, TokenService, ROLE_EDITOR,
        ROLE_VIEWER,
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
        perms: Arc<InMemoryPermissionStore>,
        revocation: Arc<InMemoryRevocationStore>,
        audit: Arc<InMemoryAuditSink>,
    }

    impl Fixture {
        fn new() -> Self {
            let perms = Arc::new(InMemoryPermissionStore::new());
            let revocation = Arc::new(InMemoryRevocationStore::new());
            let audit = Arc::new(InMemoryAuditSink::new());
            let users = Arc::new(InMemoryUserStore::new(Arc::clone(&perms)));
            let auth = AuthState::new(
                TokenService::new(SECRET),
                Arc::clone(&revocation) as Arc<dyn RevocationStore + Send + Sync>,
                Arc::clone(&perms) as Arc<dyn PermissionStore + Send + Sync>,
                Arc::clone(&audit) as Arc<dyn AuditSink + Send + Sync>,
            )
            .with_clock(Arc::new(FixedClock));
            let app = admin_router(AdminState::new(
                auth,
                Arc::clone(&users) as Arc<dyn UserStore + Send + Sync>,
            ));
            Self {
                app,
                users,
                perms,
                revocation,
                audit,
            }
        }

        async fn account(&self, email: &str, role: Role) -> String {
            match self
                .users
                .create(email, "$argon2id$h", &role.permissions)
                .await
                .unwrap()
            {
                CreateUser::Created(u) => u.id,
                CreateUser::EmailTaken => panic!("email pris"),
            }
        }

        /// Access token à la version de session courante.
        async fn token(&self, user_id: &str) -> String {
            let sv = self.revocation.session_version(user_id).await.unwrap();
            TokenService::new(SECRET)
                .issue_pair(user_id, sv, 900, 3_600, NOW)
                .unwrap()
                .0
        }

        async fn call(
            &self,
            method: &str,
            uri: &str,
            as_user: &str,
            body: Option<Value>,
        ) -> Response {
            let mut req = Request::builder().method(method).uri(uri).header(
                "Authorization",
                format!("Bearer {}", self.token(as_user).await),
            );
            let body = match body {
                Some(v) => {
                    req = req.header("Content-Type", "application/json");
                    Body::from(v.to_string())
                }
                None => Body::empty(),
            };
            self.app
                .clone()
                .oneshot(req.body(body).unwrap())
                .await
                .unwrap()
        }

        async fn actions(&self) -> Vec<(String, String, AuditOutcome)> {
            self.audit
                .list()
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.action.starts_with("admin."))
                .map(|e| (e.action, e.resource.unwrap_or_default(), e.outcome))
                .collect()
        }
    }

    async fn json_body(response: Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn text_body(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn users_admin_only() -> Role {
        Role::new("user-admin").with_permissions([PERM_USERS_ADMIN])
    }

    #[tokio::test]
    async fn every_route_requires_users_admin() {
        let fx = Fixture::new();
        let editor = fx.account("ed@example.io", Role::editor()).await;
        let other = fx.account("bob@example.io", Role::viewer()).await;
        for (method, uri, body) in [
            ("GET", "/admin/users".to_string(), None),
            ("GET", format!("/admin/users/{other}"), None),
            (
                "PUT",
                format!("/admin/users/{other}/role"),
                Some(json!({ "role": "viewer" })),
            ),
            ("POST", format!("/admin/users/{other}/disable"), None),
            ("POST", format!("/admin/users/{other}/enable"), None),
            ("POST", format!("/admin/users/{other}/logout-all"), None),
        ] {
            let res = fx.call(method, &uri, &editor, body).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {uri}");
            assert!(text_body(res).await.contains(PERM_USERS_ADMIN));
        }
        let anonymous = fx
            .app
            .clone()
            .oneshot(Request::get("/admin/users").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        assert!(!fx.users.find_by_id(&other).await.unwrap().unwrap().disabled);
    }

    #[tokio::test]
    async fn list_is_paginated_filtered_and_never_exposes_hashes() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", Role::super_admin()).await;
        fx.account("alice@example.io", Role::editor()).await;
        fx.account("bob@other.io", Role::viewer()).await;

        let res = fx.call("GET", "/admin/users", &admin, None).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = json_body(res).await;
        assert_eq!(body["total"], 3);
        assert_eq!(body["limit"], DEFAULT_PAGE_SIZE);
        let roles: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, [ROLE_ADMIN, ROLE_EDITOR, ROLE_VIEWER]);
        assert!(!body.to_string().contains("argon2"), "aucun hash exposé");

        let page = json_body(
            fx.call("GET", "/admin/users?offset=1&limit=1", &admin, None)
                .await,
        )
        .await;
        assert_eq!(page["total"], 3);
        assert_eq!(page["items"][0]["email"], "alice@example.io");

        // Filtre normalisé (casse, espaces) ; limite plafonnée.
        let filtered = json_body(
            fx.call(
                "GET",
                "/admin/users?q=%20EXAMPLE.io&limit=1000",
                &admin,
                None,
            )
            .await,
        )
        .await;
        assert_eq!(filtered["total"], 2);
        assert_eq!(filtered["limit"], MAX_PAGE_SIZE);
        assert_eq!(
            fx.actions().await,
            [
                ("admin.users_list".into(), "*".into(), AuditOutcome::Success),
                ("admin.users_list".into(), "*".into(), AuditOutcome::Success),
                ("admin.users_list".into(), "*".into(), AuditOutcome::Success),
            ]
        );
    }

    #[tokio::test]
    async fn get_returns_the_account_or_404() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", Role::super_admin()).await;
        let alice = fx.account("alice@example.io", Role::editor()).await;
        let body = json_body(
            fx.call("GET", &format!("/admin/users/{alice}"), &admin, None)
                .await,
        )
        .await;
        assert_eq!(body["email"], "alice@example.io");
        assert_eq!(body["role"], ROLE_EDITOR);
        assert_eq!(body["disabled"], false);
        let mut expected: Vec<String> = Role::editor().permissions.into_iter().collect();
        expected.sort();
        assert_eq!(body["permissions"], json!(expected));

        let res = fx.call("GET", "/admin/users/inconnu", &admin, None).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn role_change_replaces_permissions_and_takes_effect_immediately() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", Role::super_admin()).await;
        let alice = fx.account("alice@example.io", Role::editor()).await;
        let res = fx
            .call(
                "PUT",
                &format!("/admin/users/{alice}/role"),
                &admin,
                Some(json!({ "role": "viewer" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_body(res).await["role"], ROLE_VIEWER);
        assert_eq!(
            fx.perms.permissions_for(&alice).await.unwrap(),
            Role::viewer().permissions
        );
        let event = fx
            .audit
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.action == "admin.user_role")
            .unwrap();
        assert_eq!(event.actor, admin);
        assert_eq!(event.resource.as_deref(), Some(alice.as_str()));
        assert_eq!(event.metadata, json!({ "from": "editor", "to": "viewer" }));

        // Rôle inconnu ou compte inconnu.
        let res = fx
            .call(
                "PUT",
                &format!("/admin/users/{alice}/role"),
                &admin,
                Some(json!({ "role": "root" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = fx
            .call(
                "PUT",
                "/admin/users/inconnu/role",
                &admin,
                Some(json!({ "role": "viewer" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_admin_cannot_change_their_own_role_or_disable_themselves() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", Role::super_admin()).await;
        let res = fx
            .call(
                "PUT",
                &format!("/admin/users/{admin}/role"),
                &admin,
                Some(json!({ "role": "viewer" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{admin}/disable"),
                &admin,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(
            fx.perms.permissions_for(&admin).await.unwrap(),
            Role::super_admin().permissions
        );
        assert!(!fx.users.find_by_id(&admin).await.unwrap().unwrap().disabled);
        let outcomes: Vec<AuditOutcome> = fx.actions().await.into_iter().map(|a| a.2).collect();
        assert_eq!(outcomes, [AuditOutcome::Denied, AuditOutcome::Denied]);
    }

    #[tokio::test]
    async fn users_admin_without_wildcard_cannot_escalate() {
        let fx = Fixture::new();
        let manager = fx.account("manager@example.io", users_admin_only()).await;
        let root = fx.account("root@example.io", Role::super_admin()).await;
        let alice = fx.account("alice@example.io", Role::editor()).await;

        // Promouvoir quelqu'un super-admin : 403.
        let res = fx
            .call(
                "PUT",
                &format!("/admin/users/{alice}/role"),
                &manager,
                Some(json!({ "role": "admin" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            fx.perms.permissions_for(&alice).await.unwrap(),
            Role::editor().permissions
        );
        // Toucher un super-admin : 403, quelle que soit l'action.
        for (method, uri, body) in [
            (
                "PUT",
                format!("/admin/users/{root}/role"),
                Some(json!({ "role": "viewer" })),
            ),
            ("POST", format!("/admin/users/{root}/disable"), None),
            ("POST", format!("/admin/users/{root}/enable"), None),
            ("POST", format!("/admin/users/{root}/logout-all"), None),
        ] {
            let res = fx.call(method, &uri, &manager, body).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert_eq!(
            fx.perms.permissions_for(&root).await.unwrap(),
            Role::super_admin().permissions
        );
        assert_eq!(fx.revocation.session_version(&root).await.unwrap(), 0);

        // Mais gérer les autres comptes, oui.
        let res = fx
            .call(
                "PUT",
                &format!("/admin/users/{alice}/role"),
                &manager,
                Some(json!({ "role": "viewer" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        // Et un super-admin peut tout.
        let res = fx
            .call(
                "PUT",
                &format!("/admin/users/{alice}/role"),
                &root,
                Some(json!({ "role": "admin" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_body(res).await["role"], ROLE_ADMIN);
    }

    #[tokio::test]
    async fn disable_closes_every_session_and_enable_reverts() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", Role::super_admin()).await;
        let alice = fx.account("alice@example.io", Role::editor()).await;
        let before = fx.revocation.session_version(&alice).await.unwrap();

        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{alice}/disable"),
                &admin,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_body(res).await["disabled"], true);
        assert!(fx.users.find_by_id(&alice).await.unwrap().unwrap().disabled);
        assert!(fx.revocation.session_version(&alice).await.unwrap() > before);
        assert_eq!(
            fx.perms.permissions_for(&alice).await.unwrap(),
            Role::editor().permissions,
            "permissions conservées"
        );

        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{alice}/enable"),
                &admin,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_body(res).await["disabled"], false);
        assert!(!fx.users.find_by_id(&alice).await.unwrap().unwrap().disabled);

        for action in ["disable", "enable", "logout-all"] {
            let res = fx
                .call(
                    "POST",
                    &format!("/admin/users/inconnu/{action}"),
                    &admin,
                    None,
                )
                .await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{action}");
        }
        let actions: Vec<String> = fx.actions().await.into_iter().map(|a| a.0).collect();
        assert_eq!(actions, ["admin.user_disable", "admin.user_enable"]);
    }

    #[tokio::test]
    async fn promote_admin_grants_the_wildcard_and_reactivates() {
        let fx = Fixture::new();
        let alice = fx.account("alice@example.io", Role::viewer()).await;
        fx.users.set_disabled(&alice, true).await.unwrap();
        let outcome = promote_admin(
            fx.users.as_ref(),
            fx.audit.as_ref(),
            " Alice@Example.io ",
            42,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            Promotion::Promoted {
                user_id: alice.clone(),
                reactivated: true
            }
        );
        assert_eq!(
            fx.perms.permissions_for(&alice).await.unwrap(),
            Role::super_admin().permissions
        );
        assert!(!fx.users.find_by_id(&alice).await.unwrap().unwrap().disabled);
        let event = fx.audit.list().await.unwrap().pop().unwrap();
        assert_eq!(
            (event.actor.as_str(), event.action.as_str()),
            ("cli", "admin.user_promote")
        );
        assert_eq!(event.resource.as_deref(), Some(alice.as_str()));

        let outcome = promote_admin(
            fx.users.as_ref(),
            fx.audit.as_ref(),
            "nobody@example.io",
            42,
        )
        .await
        .unwrap();
        assert_eq!(outcome, Promotion::UnknownEmail);
    }

    #[tokio::test]
    async fn logout_all_bumps_the_target_session_version() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", Role::super_admin()).await;
        let alice = fx.account("alice@example.io", Role::editor()).await;
        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{alice}/logout-all"),
                &admin,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(fx.revocation.session_version(&alice).await.unwrap(), 1);
        assert_eq!(fx.revocation.session_version(&admin).await.unwrap(), 0);
        assert_eq!(
            fx.actions().await,
            [(
                "admin.user_logout_all".into(),
                alice.clone(),
                AuditOutcome::Success
            )]
        );
    }
}

//! Administration des utilisateurs (#208).
//!
//! Endpoints REST/JSON, tous protégés par la permission `users.admin` :
//!
//! | Méthode | Path                             | Body         | Succès               |
//! |---------|----------------------------------|--------------|----------------------|
//! | GET     | `/admin/users?q=&offset=&limit=` | —            | 200 + [`UserListResponse`] |
//! | POST    | `/admin/users`                   | `{ email, role }` | 201 + [`InvitedUser`] |
//! | POST    | `/admin/users/{id}/invitation`   | —            | 204                  |
//! | GET     | `/admin/users/{id}`              | —            | 200 + [`AdminUser`]  |
//! | PUT     | `/admin/users/{id}/role`         | `{ role }`   | 200 + [`AdminUser`]  |
//! | POST    | `/admin/users/{id}/disable`      | —            | 200 + [`AdminUser`]  |
//! | POST    | `/admin/users/{id}/enable`       | —            | 200 + [`AdminUser`]  |
//! | POST    | `/admin/users/{id}/logout-all`   | —            | 204                  |
//! | POST    | `/admin/users/{id}/verify-email` | —            | 200 + [`AdminUser`]  |
//! | DELETE  | `/admin/users/{id}`              | —            | 204                  |
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
//! - la **suppression** (#229, RGPD) est définitive : le compte et toutes
//!   ses données sont effacés (cf. [`crate::erasure`]), par exemple sur une
//!   demande reçue hors de l'application. Mêmes garde-fous (`*` pour un
//!   compte super-admin, 409 sur son propre compte : passer par
//!   `DELETE /auth/me`) ; l'audit désigne ensuite le compte par son
//!   pseudonyme ;
//! - la **vérification de l'email** (#231) peut être faite à la place du
//!   titulaire (lien perdu, envoi d'emails en panne) : l'adresse actuelle du
//!   compte est marquée vérifiée et sa connexion devient possible ;
//! - l'**invitation** (#241, cf. [`crate::invitation`]) crée le compte au
//!   rôle choisi (défaut `editor` ; `admin` exige `*`, comme ci-dessus), sans
//!   mot de passe utilisable, et lui envoie un lien pour le choisir. L'envoi
//!   est **synchrone** : l'administrateur sait s'il est parti
//!   (`invitation_sent`) et peut renvoyer l'invitation tant que le compte n'a
//!   pas choisi son mot de passe (409 ensuite) ; le renvoi invalide les liens
//!   précédents. Annuler une invitation, c'est supprimer le compte ;
//! - compte inconnu : 404 ; chaque appel produit un event d'audit
//!   `admin.users_list` / `admin.user_get` / `admin.user_role` /
//!   `admin.user_disable` / `admin.user_enable` / `admin.user_logout_all` /
//!   `admin.user_verify_email` / `admin.user_delete` / `admin.user_invite` /
//!   `admin.user_invite_resend` (ressource = compte visé, ou son pseudonyme une fois
//!   effacé).

use std::collections::HashSet;
use std::sync::Arc;

use autome_core::{
    has_usable_password, normalize_email, role_of, unusable_password_hash, validate_email,
    AuditEvent, AuditOutcome, AuditSink, Role, UserStore, UserSummary, PERM_USERS_ADMIN,
    PERM_WILDCARD, ROLE_ADMIN, ROLE_EDITOR,
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
use crate::erasure::{erase_account, Erasure};
use crate::invitation::{invite_account, Invitation, Invited};

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
    /// Suppression de compte (#229) ; `None` = indisponible (503).
    pub erasure: Option<Erasure>,
    /// Invitations (#241) ; `None` = indisponibles (503), faute d'envoi
    /// d'email configuré.
    pub invitation: Option<Invitation>,
}

impl AdminState {
    /// Construit l'état.
    pub fn new(auth: AuthState, users: Arc<dyn UserStore + Send + Sync>) -> Self {
        Self {
            auth,
            users,
            erasure: None,
            invitation: None,
        }
    }

    /// Active les invitations de comptes (#241).
    pub fn with_invitation(mut self, invitation: Invitation) -> Self {
        self.invitation = Some(invitation);
        self
    }

    /// Active la suppression de comptes (#229).
    pub fn with_erasure(mut self, erasure: Erasure) -> Self {
        self.erasure = Some(erasure);
        self
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
    /// Adresse email vérifiée (#231).
    pub email_verified: bool,
    /// Invitation en attente (#241) : le compte n'a pas encore choisi son mot
    /// de passe et ne peut pas se connecter.
    pub invitation_pending: bool,
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
            email_verified: user.email_verified,
            invitation_pending: !user.password_set,
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

/// Corps de [`invite_user`].
#[derive(Debug, Deserialize, ToSchema)]
pub struct InviteBody {
    /// Adresse de la personne invitée (normalisée côté serveur).
    pub email: String,
    /// `admin`, `editor` ou `viewer` ; défaut `editor`.
    #[serde(default)]
    pub role: Option<String>,
}

/// Réponse de [`invite_user`].
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InvitedUser {
    /// Compte créé (invitation en attente).
    pub user: AdminUser,
    /// L'email d'invitation est parti ; sinon, le renvoyer
    /// (`POST /admin/users/{id}/invitation`).
    pub invitation_sent: bool,
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
    /// Fonction non configurée sur ce déploiement.
    Unavailable(&'static str),
    /// Service externe en échec (envoi d'email).
    BadGateway(&'static str),
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
            AdminError::Unavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg).into_response(),
            AdminError::BadGateway(msg) => (StatusCode::BAD_GATEWAY, msg).into_response(),
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

/// `POST /admin/users` — invite une personne : compte créé au rôle choisi,
/// sans mot de passe utilisable, et email d'invitation envoyé.
#[utoipa::path(
    post,
    path = "/admin/users",
    tag = "admin",
    request_body = InviteBody,
    responses(
        (status = 201, description = "Compte créé, invitation en attente (`invitation_sent` : email parti)", body = InvitedUser),
        (status = 400, description = "Email invalide ou rôle inconnu"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`, ou `*` pour le rôle `admin`"),
        (status = 409, description = "Email déjà enregistré"),
        (status = 503, description = "Invitations indisponibles : aucun envoi d'email configuré")
    ),
    security(("bearer" = []))
)]
pub async fn invite_user(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Json(body): Json<InviteBody>,
) -> Result<(StatusCode, Json<InvitedUser>), AdminError> {
    const ACTION: &str = "admin.user_invite";
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    let Some(invitation) = state.invitation.as_ref() else {
        return Err(AdminError::Unavailable("invitations are not configured"));
    };
    let email = normalize_email(&body.email);
    validate_email(&email).map_err(|e| AdminError::BadRequest(e.to_string()))?;
    let Some(role) = Role::standard(body.role.as_deref().unwrap_or(ROLE_EDITOR)) else {
        return Err(AdminError::BadRequest(
            "unknown role (expected admin, editor or viewer)".into(),
        ));
    };
    if role.name == ROLE_ADMIN {
        require_wildcard(&state, &user, &email, ACTION).await?;
    }
    let now = state.auth.clock.now_unix_seconds();
    let (invited, sent) = match invite_account(
        state.users.as_ref(),
        invitation,
        &email,
        &role.permissions,
        now,
    )
    .await
    .map_err(internal)?
    {
        Invited::Created { user, sent } => (user, sent),
        Invited::EmailTaken => {
            audit(
                &state,
                user.user_id(),
                ACTION,
                &email,
                AuditOutcome::Denied,
                json!({ "reason": "email taken" }),
            )
            .await;
            return Err(AdminError::Conflict("email already registered"));
        }
    };
    audit(
        &state,
        user.user_id(),
        ACTION,
        &invited.id,
        AuditOutcome::Success,
        json!({ "role": role.name, "sent": sent }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(InvitedUser {
            user: reload(&state, &invited.id).await?,
            invitation_sent: sent,
        }),
    ))
}

/// `POST /admin/users/{id}/invitation` — renvoie l'invitation d'un compte qui
/// n'a pas encore choisi son mot de passe ; les liens précédents ne valent
/// plus rien.
#[utoipa::path(
    post,
    path = "/admin/users/{id}/invitation",
    tag = "admin",
    params(("id" = String, Path, description = "Identifiant du compte")),
    responses(
        (status = 204, description = "Nouvelle invitation envoyée"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`, ou `*` pour un compte super-admin"),
        (status = 404, description = "Compte inconnu"),
        (status = 409, description = "Compte déjà activé (mot de passe choisi) ou suspendu"),
        (status = 502, description = "Envoi de l'email impossible ; les liens précédents sont invalidés"),
        (status = 503, description = "Invitations indisponibles : aucun envoi d'email configuré")
    ),
    security(("bearer" = []))
)]
pub async fn resend_invitation(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AdminError> {
    const ACTION: &str = "admin.user_invite_resend";
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    let Some(invitation) = state.invitation.as_ref() else {
        return Err(AdminError::Unavailable("invitations are not configured"));
    };
    target_for_change(&state, &user, &id, ACTION).await?;
    let Some(account) = state.users.find_by_id(&id).await.map_err(internal)? else {
        return Err(AdminError::NotFound);
    };
    if has_usable_password(&account.password_hash) {
        return Err(AdminError::Conflict("account already activated"));
    }
    if account.disabled {
        return Err(AdminError::Conflict("account disabled"));
    }
    // Nouveau hash inutilisable : les liens liés à l'ancien meurent. Écriture
    // conditionnelle : une acceptation concurrente gagne.
    let rotated = autome_core::UserRecord {
        password_hash: unusable_password_hash(),
        ..account.clone()
    };
    if !state
        .users
        .update_password(&id, &account.password_hash, &rotated.password_hash)
        .await
        .map_err(internal)?
    {
        return Err(AdminError::Conflict("account already activated"));
    }
    let now = state.auth.clock.now_unix_seconds();
    if let Err(e) = invitation.send(&rotated, now).await {
        tracing::error!(target_id = %id, error = %e, "renvoi de l'invitation impossible");
        audit(
            &state,
            user.user_id(),
            ACTION,
            &id,
            AuditOutcome::Failure,
            json!({ "reason": "mail not sent" }),
        )
        .await;
        return Err(AdminError::BadGateway("invitation email not sent"));
    }
    tracing::info!(target_id = %id, "invitation renvoyée");
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

/// `POST /admin/users/{id}/verify-email` — marque vérifiée l'adresse actuelle
/// du compte (#231) : sa connexion devient possible. Idempotent.
#[utoipa::path(
    post,
    path = "/admin/users/{id}/verify-email",
    tag = "admin",
    params(("id" = String, Path, description = "Identifiant du compte")),
    responses(
        (status = 200, description = "Adresse vérifiée", body = AdminUser),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`, ou `*` pour un compte super-admin"),
        (status = 404, description = "Compte inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn verify_user_email(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Json<AdminUser>, AdminError> {
    const ACTION: &str = "admin.user_verify_email";
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    let target = target_for_change(&state, &user, &id, ACTION).await?;
    if !state
        .users
        .mark_email_verified(&id, &target.email)
        .await
        .map_err(internal)?
    {
        // Effacé, ou son adresse a changé entre la lecture et l'écriture.
        return Err(AdminError::NotFound);
    }
    tracing::info!(target_id = %id, "adresse email vérifiée par un administrateur");
    audit(
        &state,
        user.user_id(),
        ACTION,
        &id,
        AuditOutcome::Success,
        json!({ "already": target.email_verified }),
    )
    .await;
    Ok(Json(reload(&state, &id).await?))
}

/// `DELETE /admin/users/{id}` — efface définitivement le compte et toutes
/// ses données (RGPD, droit à l'effacement) ; ses sessions sont fermées.
#[utoipa::path(
    delete,
    path = "/admin/users/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Identifiant du compte")),
    responses(
        (status = 204, description = "Compte et données effacés"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `users.admin`, ou `*` pour un compte super-admin"),
        (status = 404, description = "Compte inconnu"),
        (status = 409, description = "Son propre compte (passer par `DELETE /auth/me`)"),
        (status = 503, description = "Suppression indisponible sur ce déploiement")
    ),
    security(("bearer" = []))
)]
pub async fn delete_user(
    user: AuthenticatedUser,
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AdminError> {
    const ACTION: &str = "admin.user_delete";
    state
        .auth
        .require_permission(&user, PERM_USERS_ADMIN)
        .await?;
    let Some(erasure) = state.erasure.clone() else {
        return Err(AdminError::Unavailable(
            "account deletion is not configured",
        ));
    };
    if id == user.user_id() {
        return Err(
            refuse_self(&state, &user, ACTION, "cannot delete your own account here").await,
        );
    }
    target_for_change(&state, &user, &id, ACTION).await?;
    let Some(account) = state.users.find_by_id(&id).await.map_err(internal)? else {
        return Err(AdminError::NotFound);
    };
    let Some(erased) = erase_account(&state.auth, &erasure, &account)
        .await
        .map_err(internal)?
    else {
        return Err(AdminError::NotFound);
    };
    audit(
        &state,
        user.user_id(),
        ACTION,
        &erased.pseudonym,
        AuditOutcome::Success,
        json!({
            "workflows": erased.report.workflows,
            "runs": erased.report.runs,
            "connector_tokens": erased.report.connector_tokens,
        }),
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
/// `admin` (wildcard `*`) au compte de `email`, le réactive s'il était
/// suspendu et vérifie son adresse (#231 : sans quoi il ne pourrait pas se
/// connecter si l'envoi d'emails est en panne) — outil de création du premier administrateur et de
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
    if !account.email_verified {
        users
            .mark_email_verified(&account.id, &account.email)
            .await?;
    }
    let event = AuditEvent::new(now_ms, "cli", "admin.user_promote", AuditOutcome::Success)
        .with_resource(&account.id)
        .with_metadata(json!({
            "reactivated": account.disabled,
            "email_verified": !account.email_verified,
        }));
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
        .route("/admin/users", get(list_users).post(invite_user))
        .route("/admin/users/:id", get(get_user).delete(delete_user))
        .route("/admin/users/:id/role", put(set_role))
        .route("/admin/users/:id/disable", post(disable_user))
        .route("/admin/users/:id/enable", post(enable_user))
        .route("/admin/users/:id/logout-all", post(logout_user))
        .route("/admin/users/:id/verify-email", post(verify_user_email))
        .route("/admin/users/:id/invitation", post(resend_invitation))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::erasure::InMemoryAccountEraser;
    use crate::invitation::InviteTokens;
    use crate::mail::{MailSettings, Mailer, MemoryMailer, OutgoingMail};
    use autome_connectors::oauth::InMemoryConnectorTokenStore;
    use autome_core::{
        AuditSink, CreateUser, EnvelopeCipher, InMemoryAuditSink, InMemoryKeyProvider,
        InMemoryLoginThrottle, InMemoryPermissionStore, InMemoryRevocationStore,
        InMemoryRunStateStore, InMemoryTriggerScheduleStore, InMemoryUserStore,
        InMemoryWebhookEndpointStore, InMemoryWorkflowRepository, Pagination, PermissionStore,
        RevocationStore, TokenService, WorkflowGraph, WorkflowId, WorkflowRepository,
        ERASED_PREFIX, ROLE_EDITOR, ROLE_VIEWER,
    };
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const NOW: i64 = 1_700_000_000;

    struct FixedClock;
    impl crate::auth::Clock for FixedClock {
        fn now_unix_seconds(&self) -> i64 {
            NOW
        }
    }

    /// Transport d'emails qu'un test peut mettre en panne.
    #[derive(Default)]
    struct ToggleMailer {
        sent: MemoryMailer,
        down: AtomicBool,
    }

    #[axum::async_trait]
    impl Mailer for ToggleMailer {
        async fn send(&self, mail: &OutgoingMail) -> Result<(), String> {
            if self.down.load(Ordering::SeqCst) {
                return Err("smtp en panne".into());
            }
            self.sent.send(mail).await
        }
    }

    const INVITE_KEY: &[u8] = b"test-encryption-key";

    struct Fixture {
        app: Router,
        auth: AuthState,
        mailer: Arc<ToggleMailer>,
        users: Arc<InMemoryUserStore>,
        perms: Arc<InMemoryPermissionStore>,
        revocation: Arc<InMemoryRevocationStore>,
        audit: Arc<InMemoryAuditSink>,
        workflows: Arc<InMemoryWorkflowRepository>,
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
            let workflows = Arc::new(InMemoryWorkflowRepository::new());
            let eraser = InMemoryAccountEraser::new(
                Arc::clone(&users),
                workflows.clone(),
                Arc::new(InMemoryRunStateStore::new()),
                Arc::new(InMemoryConnectorTokenStore::new(EnvelopeCipher::new(
                    InMemoryKeyProvider::with_key("v1", [4u8; 32]),
                ))),
                Arc::new(InMemoryWebhookEndpointStore::new()),
                Arc::new(InMemoryTriggerScheduleStore::new()),
                Some(Arc::clone(&audit)),
            );
            let erasure = Erasure::new(Arc::new(eraser), Arc::new(InMemoryLoginThrottle::new()));
            let mailer = Arc::new(ToggleMailer::default());
            let invitation = Invitation::new(
                InviteTokens::new(INVITE_KEY),
                MailSettings {
                    mailer: mailer.clone(),
                    public_url: "https://app.example.io".into(),
                },
            );
            let app = admin_router(
                AdminState::new(
                    auth.clone(),
                    Arc::clone(&users) as Arc<dyn UserStore + Send + Sync>,
                )
                .with_erasure(erasure)
                .with_invitation(invitation),
            );
            Self {
                app,
                auth,
                mailer,
                users,
                perms,
                revocation,
                audit,
                workflows,
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
            ("DELETE", format!("/admin/users/{other}"), None),
            (
                "POST",
                "/admin/users".to_string(),
                Some(json!({ "email": "new@example.io" })),
            ),
            ("POST", format!("/admin/users/{other}/invitation"), None),
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
    async fn an_administrator_can_verify_an_address() {
        let fx = Fixture::new();
        let manager = fx.account("manager@example.io", users_admin_only()).await;
        let root = fx.account("root@example.io", Role::super_admin()).await;
        let CreateUser::Created(alice) = fx
            .users
            .create(
                "alice@example.io",
                "$argon2id$h",
                &Role::editor().permissions,
                false,
            )
            .await
            .unwrap()
        else {
            panic!("création");
        };
        let res = fx
            .call("GET", &format!("/admin/users/{}", alice.id), &manager, None)
            .await;
        assert_eq!(json_body(res).await["email_verified"], false);

        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{}/verify-email", alice.id),
                &manager,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_body(res).await["email_verified"], true);
        assert!(
            fx.users
                .find_by_id(&alice.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        let event = fx.audit.list().await.unwrap().pop().unwrap();
        assert_eq!(event.action, "admin.user_verify_email");
        assert_eq!(event.actor, manager);
        assert_eq!(event.resource.as_deref(), Some(alice.id.as_str()));

        // Super-admin visé sans `*` : 403 ; compte inconnu : 404.
        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{root}/verify-email"),
                &manager,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = fx
            .call(
                "POST",
                "/admin/users/00000000-0000-0000-0000-00000000dead/verify-email",
                &manager,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn promote_admin_also_verifies_the_address() {
        let fx = Fixture::new();
        let CreateUser::Created(alice) = fx
            .users
            .create(
                "alice@example.io",
                "$argon2id$h",
                &Role::viewer().permissions,
                false,
            )
            .await
            .unwrap()
        else {
            panic!("création");
        };
        promote_admin(fx.users.as_ref(), fx.audit.as_ref(), "alice@example.io", 42)
            .await
            .unwrap();
        assert!(
            fx.users
                .find_by_id(&alice.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        let event = fx.audit.list().await.unwrap().pop().unwrap();
        assert_eq!(event.metadata["email_verified"], true);
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

    // ───── Suppression (#229) ─────

    #[tokio::test]
    async fn an_admin_erases_an_account_and_its_data() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", users_admin_only()).await;
        let alice = fx.account("alice@example.io", Role::editor()).await;
        fx.workflows
            .create(
                &alice,
                WorkflowGraph {
                    id: WorkflowId::new("wf"),
                    name: "wf".into(),
                    nodes: vec![],
                    edges: vec![],
                },
            )
            .await
            .unwrap();
        // Une action d'administration antérieure qui vise alice.
        fx.call("GET", &format!("/admin/users/{alice}"), &admin, None)
            .await;

        let res = fx
            .call("DELETE", &format!("/admin/users/{alice}"), &admin, None)
            .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(fx.users.find_by_id(&alice).await.unwrap(), None);
        assert_eq!(
            fx.workflows
                .list(&alice, Pagination::default_paging())
                .await
                .unwrap()
                .total,
            0
        );
        assert!(
            fx.revocation.session_version(&alice).await.unwrap() > 0,
            "sessions fermées"
        );

        let actions = fx.actions().await;
        assert!(actions.iter().all(|(_, resource, _)| resource != &alice));
        let (_, pseudonym, outcome) = actions
            .iter()
            .find(|(action, _, _)| action == "admin.user_delete")
            .expect("suppression auditée");
        assert_eq!(*outcome, AuditOutcome::Success);
        assert!(pseudonym.starts_with(ERASED_PREFIX));
        assert!(
            actions
                .iter()
                .any(|(action, resource, _)| action == "admin.user_get" && resource == pseudonym),
            "action antérieure rattachée au pseudonyme"
        );

        let again = fx
            .call("DELETE", &format!("/admin/users/{alice}"), &admin, None)
            .await;
        assert_eq!(again.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn deletion_keeps_the_self_and_super_admin_guards() {
        let fx = Fixture::new();
        let admin = fx.account("admin@example.io", users_admin_only()).await;
        let root = fx.account("root@example.io", Role::super_admin()).await;

        let own = fx
            .call("DELETE", &format!("/admin/users/{admin}"), &admin, None)
            .await;
        assert_eq!(own.status(), StatusCode::CONFLICT);
        let super_admin = fx
            .call("DELETE", &format!("/admin/users/{root}"), &admin, None)
            .await;
        assert_eq!(super_admin.status(), StatusCode::FORBIDDEN);
        assert!(fx.users.find_by_id(&admin).await.unwrap().is_some());
        assert!(fx.users.find_by_id(&root).await.unwrap().is_some());

        // Un super-admin peut effacer un autre super-admin (il en reste un).
        let other_root = fx.account("root2@example.io", Role::super_admin()).await;
        let res = fx
            .call("DELETE", &format!("/admin/users/{other_root}"), &root, None)
            .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let unknown = fx.call("DELETE", "/admin/users/inconnu", &root, None).await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    // --- Invitations (#241) -------------------------------------------------

    /// Jeton du dernier email d'invitation envoyé.
    fn last_invite_token(fx: &Fixture) -> String {
        let mail = fx.mailer.sent.sent().pop().expect("invitation envoyée");
        let (_, rest) = mail.body.split_once("#invite=").expect("lien attendu");
        rest.split_whitespace().next().unwrap().to_string()
    }

    #[tokio::test]
    async fn an_admin_invites_with_a_role_and_the_link_targets_the_account() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", users_admin_only()).await;
        let res = fx
            .call(
                "POST",
                "/admin/users",
                &admin,
                Some(json!({ "email": " Ivy@Example.io ", "role": "viewer" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        let body = json_body(res).await;
        assert_eq!(body["invitation_sent"], true);
        let user = &body["user"];
        assert_eq!(user["email"], "ivy@example.io");
        assert_eq!(user["role"], ROLE_VIEWER);
        assert_eq!(user["invitation_pending"], true);
        assert_eq!(user["email_verified"], false);
        assert!(user.get("password_hash").is_none());
        let id = user["id"].as_str().unwrap().to_string();

        let mail = fx.mailer.sent.sent().pop().unwrap();
        assert_eq!(mail.to, "ivy@example.io");
        assert!(mail.body.contains("https://app.example.io/#invite="));
        let token = last_invite_token(&fx);
        let parsed = InviteTokens::parse(&token).unwrap();
        assert_eq!(parsed.user_id, id);
        let account = fx.users.find_by_id(&id).await.unwrap().unwrap();
        assert!(!has_usable_password(&account.password_hash));
        assert!(InviteTokens::new(INVITE_KEY).verify(&parsed, &account.password_hash, NOW));

        // Rôle par défaut : editor ; email déjà pris : 409.
        let res = fx
            .call(
                "POST",
                "/admin/users",
                &admin,
                Some(json!({ "email": "joe@example.io" })),
            )
            .await;
        assert_eq!(json_body(res).await["user"]["role"], ROLE_EDITOR);
        let res = fx
            .call(
                "POST",
                "/admin/users",
                &admin,
                Some(json!({ "email": "ivy@example.io" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(fx.mailer.sent.sent().len(), 2, "pas d'email sur un conflit");

        let invites: Vec<_> = fx
            .actions()
            .await
            .into_iter()
            .filter(|(a, _, _)| a == "admin.user_invite")
            .collect();
        assert_eq!(invites.len(), 3);
        assert_eq!(
            invites[0],
            ("admin.user_invite".into(), id, AuditOutcome::Success)
        );
        assert_eq!(invites[2].2, AuditOutcome::Denied);
    }

    #[tokio::test]
    async fn invitations_validate_input_and_never_escalate() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", users_admin_only()).await;
        for (body, status) in [
            (json!({ "email": "pas-un-email" }), StatusCode::BAD_REQUEST),
            (
                json!({ "email": "a@example.io", "role": "owner" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "email": "a@example.io", "role": "admin" }),
                StatusCode::FORBIDDEN,
            ),
        ] {
            let res = fx
                .call("POST", "/admin/users", &admin, Some(body.clone()))
                .await;
            assert_eq!(res.status(), status, "{body}");
        }
        assert!(fx
            .users
            .find_by_email("a@example.io")
            .await
            .unwrap()
            .is_none());
        assert!(fx.mailer.sent.sent().is_empty());

        // Avec `*`, le rôle admin est attribuable dès l'invitation.
        let root = fx.account("super@example.io", Role::super_admin()).await;
        let res = fx
            .call(
                "POST",
                "/admin/users",
                &root,
                Some(json!({ "email": "a@example.io", "role": "admin" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        assert_eq!(json_body(res).await["user"]["role"], ROLE_ADMIN);
        // Et seul `*` peut renvoyer l'invitation d'un futur super-admin.
        let id = fx
            .users
            .find_by_email("a@example.io")
            .await
            .unwrap()
            .unwrap()
            .id;
        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{id}/invitation"),
                &admin,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_failed_mail_keeps_the_account_and_the_invitation_can_be_resent() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", users_admin_only()).await;
        fx.mailer.down.store(true, Ordering::SeqCst);
        let res = fx
            .call(
                "POST",
                "/admin/users",
                &admin,
                Some(json!({ "email": "ivy@example.io" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        let body = json_body(res).await;
        assert_eq!(body["invitation_sent"], false);
        let id = body["user"]["id"].as_str().unwrap().to_string();
        let uri = format!("/admin/users/{id}/invitation");

        // Renvoi toujours en panne : 502.
        let res = fx.call("POST", &uri, &admin, None).await;
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);

        fx.mailer.down.store(false, Ordering::SeqCst);
        let res = fx.call("POST", &uri, &admin, None).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let first = last_invite_token(&fx);
        let res = fx.call("POST", &uri, &admin, None).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let second = last_invite_token(&fx);

        // Seul le dernier lien vaut.
        let hash = fx
            .users
            .find_by_id(&id)
            .await
            .unwrap()
            .unwrap()
            .password_hash;
        let tokens = InviteTokens::new(INVITE_KEY);
        assert!(!tokens.verify(&InviteTokens::parse(&first).unwrap(), &hash, NOW));
        assert!(tokens.verify(&InviteTokens::parse(&second).unwrap(), &hash, NOW));

        let outcomes: Vec<_> = fx
            .actions()
            .await
            .into_iter()
            .filter(|(a, _, _)| a == "admin.user_invite_resend")
            .map(|(_, _, o)| o)
            .collect();
        assert_eq!(
            outcomes,
            [
                AuditOutcome::Failure,
                AuditOutcome::Success,
                AuditOutcome::Success
            ]
        );
    }

    #[tokio::test]
    async fn only_pending_and_active_invitations_can_be_resent() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", users_admin_only()).await;
        let activated = fx.account("bob@example.io", Role::editor()).await;
        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{activated}/invitation"),
                &admin,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(text_body(res).await, "account already activated");

        let res = fx
            .call(
                "POST",
                "/admin/users",
                &admin,
                Some(json!({ "email": "ivy@example.io" })),
            )
            .await;
        let id = json_body(res).await["user"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        fx.call("POST", &format!("/admin/users/{id}/disable"), &admin, None)
            .await;
        let res = fx
            .call(
                "POST",
                &format!("/admin/users/{id}/invitation"),
                &admin,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(text_body(res).await, "account disabled");

        let res = fx
            .call("POST", "/admin/users/inconnu/invitation", &admin, None)
            .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            fx.mailer.sent.sent().len(),
            1,
            "seule l'invitation initiale"
        );

        // Annuler une invitation : supprimer le compte.
        let res = fx
            .call("DELETE", &format!("/admin/users/{id}"), &admin, None)
            .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(fx.users.find_by_id(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn without_mail_invitations_are_503() {
        let fx = Fixture::new();
        let admin = fx.account("root@example.io", users_admin_only()).await;
        let other = fx.account("bob@example.io", Role::editor()).await;
        let state = AdminState::new(fx.auth.clone(), fx.users.clone());
        let app = admin_router(state);
        for (uri, body) in [
            (
                "/admin/users".to_string(),
                json!({ "email": "a@example.io" }),
            ),
            (format!("/admin/users/{other}/invitation"), Value::Null),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::post(&uri)
                        .header(
                            "Authorization",
                            format!("Bearer {}", fx.token(&admin).await),
                        )
                        .header("Content-Type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        }
    }
}

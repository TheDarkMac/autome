//! Endpoints credentials / connecteurs (F7.3.b/c).
//!
//! Endpoints REST/JSON :
//!
//! | Méthode | Path                                  | Permission            | Description                              |
//! |---------|---------------------------------------|-----------------------|------------------------------------------|
//! | GET     | `/credentials/providers`              | `credentials.manage`  | Liste les providers OAuth déclarés       |
//! | POST    | `/credentials/{provider}/authorize`   | `credentials.manage`  | Démarre un flow OAuth (URL + state + PKCE) |
//! | POST    | `/credentials/{provider}/callback`    | `credentials.manage`  | Échange `code` → tokens chiffrés         |
//! | GET     | `/credentials`                        | `credentials.manage`  | Résumé des credentials de l'utilisateur  |
//! | DELETE  | `/credentials/{provider}`             | `credentials.manage`  | Révoque un credential                    |
//!
//! Toutes les routes :
//! - sont protégées par [`crate::auth::AuthenticatedUser`] + check
//!   `require_permission(PERM_CREDENTIALS_MANAGE)` ;
//! - enregistrent un event audit `credentials.<verb>` par appel (actor =
//!   user_id, resource = provider_id ou `"*"` pour les listes).
//!
//! Aucune route ne retourne de token brut ni de `client_secret` — seules
//! des métadonnées non sensibles ([`autome_connectors::CredentialSummary`],
//! [`autome_connectors::ProviderInfo`]) traversent la frontière HTTP.

use std::sync::Arc;

use autome_connectors::CredentialService;
use autome_core::{AuditEvent, AuditOutcome, PERM_CREDENTIALS_MANAGE};
use axum::{
    extract::{FromRef, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{AuthError, AuthState, AuthenticatedUser};

/// État partagé par les handlers credentials : auth + service.
#[derive(Clone)]
pub struct CredentialsState {
    /// État d'authentification (token, perms, clock, audit).
    pub auth: AuthState,
    /// Service qui orchestre `OAuthClient` + `ConnectorTokenStore`.
    pub service: Arc<dyn CredentialService + Send + Sync>,
}

impl CredentialsState {
    /// Builder convenience.
    pub fn new(auth: AuthState, service: Arc<dyn CredentialService + Send + Sync>) -> Self {
        Self { auth, service }
    }
}

impl FromRef<CredentialsState> for AuthState {
    fn from_ref(input: &CredentialsState) -> Self {
        input.auth.clone()
    }
}

// ───────────────────────── DTO ─────────────────────────

/// Description publique d'un provider — pas de `client_secret`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderDto {
    /// Identifiant stable du provider.
    pub provider_id: String,
    /// Scopes demandés.
    pub scopes: Vec<String>,
}

/// Réponse de `GET /credentials/providers`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ListProvidersResponse {
    /// Providers déclarés, tri lexical par `provider_id`.
    pub items: Vec<ProviderDto>,
}

/// Réponse de `POST /credentials/{provider}/authorize` (PKCE côté SPA :
/// le caller conserve `state` + `code_verifier` jusqu'au callback).
#[derive(Debug, Serialize, ToSchema)]
pub struct AuthorizeResponse {
    /// URL absolue où rediriger l'utilisateur.
    pub authorize_url: String,
    /// Paramètre `state` (anti-CSRF) à comparer dans le callback.
    pub state: String,
    /// PKCE verifier — à reposter à `/callback`.
    pub code_verifier: String,
}

/// Body de `POST /credentials/{provider}/callback`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CallbackBody {
    /// `code` retourné par le provider à l'utilisateur.
    pub code: String,
    /// `state` retourné par le provider — anti-CSRF, identique à
    /// celui renvoyé par `/authorize`.
    pub state: String,
    /// PKCE verifier que le caller a conservé entre `/authorize` et ici.
    pub code_verifier: String,
}

/// Résumé d'un credential — jamais le token brut.
#[derive(Debug, Serialize, ToSchema)]
pub struct CredentialDto {
    /// Provider concerné.
    pub provider_id: String,
    /// `true` si un refresh token est conservé.
    pub has_refresh_token: bool,
    /// Scopes effectivement accordés (verbatim).
    pub scope: Option<String>,
    /// Timestamp d'expiration de l'access token (Unix ms).
    pub expires_at_ms: Option<i64>,
}

impl From<autome_connectors::CredentialSummary> for CredentialDto {
    fn from(s: autome_connectors::CredentialSummary) -> Self {
        Self {
            provider_id: s.provider_id,
            has_refresh_token: s.has_refresh_token,
            scope: s.scope,
            expires_at_ms: s.expires_at_ms,
        }
    }
}

/// Réponse de `GET /credentials`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ListCredentialsResponse {
    /// Credentials du user courant, tri lexical par `provider_id`.
    pub items: Vec<CredentialDto>,
}

// ───────────────────────── erreur ─────────────────────────

/// Erreur métier des handlers credentials.
#[derive(Debug)]
pub enum CredentialError {
    /// Auth/RBAC (propagation depuis le middleware).
    Auth(AuthError),
    /// Provider inconnu / credential introuvable.
    NotFound(String),
    /// Erreur OAuth (échec d'exchange, refus du provider).
    Upstream(String),
    /// Erreur backend (store, transport).
    Internal(String),
}

impl From<AuthError> for CredentialError {
    fn from(e: AuthError) -> Self {
        Self::Auth(e)
    }
}

impl IntoResponse for CredentialError {
    fn into_response(self) -> Response {
        match self {
            CredentialError::Auth(e) => e.into_response(),
            CredentialError::NotFound(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
            CredentialError::Upstream(msg) => (StatusCode::BAD_GATEWAY, msg).into_response(),
            CredentialError::Internal(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        }
    }
}

async fn audit_event(
    state: &CredentialsState,
    actor: &str,
    action: &str,
    resource: &str,
    outcome: AuditOutcome,
) {
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let event = AuditEvent::new(now_ms, actor, action, outcome).with_resource(resource);
    if let Err(e) = state.auth.audit.record(event).await {
        eprintln!("[credentials] audit record failed: {e}");
    }
}

// ───────────────────────── handlers ─────────────────────────

/// `GET /credentials/providers` — liste les providers OAuth.
#[utoipa::path(
    get,
    path = "/credentials/providers",
    tag = "credentials",
    responses(
        (status = 200, description = "Providers OAuth déclarés (sans `client_secret`)", body = ListProvidersResponse),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `credentials.manage`")
    ),
    security(("bearer" = []))
)]
pub async fn list_providers(
    user: AuthenticatedUser,
    State(state): State<CredentialsState>,
) -> Result<Json<ListProvidersResponse>, CredentialError> {
    state
        .auth
        .require_permission(&user, PERM_CREDENTIALS_MANAGE)
        .await?;
    let items: Vec<ProviderDto> = state
        .service
        .list_providers()
        .into_iter()
        .map(|p| ProviderDto {
            provider_id: p.provider_id,
            scopes: p.scopes,
        })
        .collect();
    audit_event(
        &state,
        user.user_id(),
        "credentials.providers",
        "*",
        AuditOutcome::Success,
    )
    .await;
    Ok(Json(ListProvidersResponse { items }))
}

/// `POST /credentials/{provider}/authorize` — démarre un flow OAuth.
#[utoipa::path(
    post,
    path = "/credentials/{provider}/authorize",
    tag = "credentials",
    params(("provider" = String, Path, description = "Identifiant du provider OAuth")),
    responses(
        (status = 200, description = "URL d'autorisation + PKCE", body = AuthorizeResponse),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `credentials.manage`"),
        (status = 404, description = "Provider inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn start_authorize(
    user: AuthenticatedUser,
    State(state): State<CredentialsState>,
    Path(provider_id): Path<String>,
) -> Result<Json<AuthorizeResponse>, CredentialError> {
    state
        .auth
        .require_permission(&user, PERM_CREDENTIALS_MANAGE)
        .await?;
    match state.service.start_authorize(&provider_id) {
        Ok(start) => {
            audit_event(
                &state,
                user.user_id(),
                "credentials.authorize",
                &provider_id,
                AuditOutcome::Success,
            )
            .await;
            Ok(Json(AuthorizeResponse {
                authorize_url: start.authorize_url,
                state: start.state,
                code_verifier: start.code_verifier,
            }))
        }
        Err(e) => {
            audit_event(
                &state,
                user.user_id(),
                "credentials.authorize",
                &provider_id,
                AuditOutcome::Failure,
            )
            .await;
            Err(CredentialError::NotFound(format!(
                "provider '{provider_id}' inconnu : {e}"
            )))
        }
    }
}

/// `POST /credentials/{provider}/callback` — échange `code` → tokens.
#[utoipa::path(
    post,
    path = "/credentials/{provider}/callback",
    tag = "credentials",
    params(("provider" = String, Path, description = "Identifiant du provider OAuth")),
    request_body(content = CallbackBody, description = "`code` + `state` + `code_verifier` du callback OAuth"),
    responses(
        (status = 200, description = "Tokens persistés (chiffrés) — résumé safe", body = CredentialDto),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `credentials.manage`"),
        (status = 502, description = "Échec OAuth en amont (rejet du provider)")
    ),
    security(("bearer" = []))
)]
pub async fn complete_authorize(
    user: AuthenticatedUser,
    State(state): State<CredentialsState>,
    Path(provider_id): Path<String>,
    Json(body): Json<CallbackBody>,
) -> Result<Json<CredentialDto>, CredentialError> {
    state
        .auth
        .require_permission(&user, PERM_CREDENTIALS_MANAGE)
        .await?;
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let _ = body.state; // F7.3 : le caller a déjà comparé state ; on l'accepte tel quel.

    // L'échange OAuth fait un appel HTTP **bloquant** (`HttpClient` sync) : on
    // l'exécute via `spawn_blocking` pour ne pas bloquer — ni paniquer — sur le
    // runtime tokio (dette #107). Données clonées car déplacées dans la tâche.
    let service = state.service.clone();
    let (uid, pid, code, verifier) = (
        user.user_id().to_string(),
        provider_id.clone(),
        body.code.clone(),
        body.code_verifier.clone(),
    );
    let result = tokio::task::spawn_blocking(move || {
        service.complete_authorize(&uid, &pid, &code, &verifier, now_ms)
    })
    .await
    .map_err(|e| CredentialError::Internal(format!("tâche OAuth annulée : {e}")))?;

    match result {
        Ok(summary) => {
            audit_event(
                &state,
                user.user_id(),
                "credentials.callback",
                &provider_id,
                AuditOutcome::Success,
            )
            .await;
            Ok(Json(summary.into()))
        }
        Err(e) => {
            audit_event(
                &state,
                user.user_id(),
                "credentials.callback",
                &provider_id,
                AuditOutcome::Failure,
            )
            .await;
            Err(CredentialError::Upstream(e.to_string()))
        }
    }
}

/// `GET /credentials` — résumé des credentials de l'utilisateur courant.
#[utoipa::path(
    get,
    path = "/credentials",
    tag = "credentials",
    responses(
        (status = 200, description = "Credentials de l'utilisateur (sans tokens bruts)", body = ListCredentialsResponse),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `credentials.manage`")
    ),
    security(("bearer" = []))
)]
pub async fn list_credentials(
    user: AuthenticatedUser,
    State(state): State<CredentialsState>,
) -> Result<Json<ListCredentialsResponse>, CredentialError> {
    state
        .auth
        .require_permission(&user, PERM_CREDENTIALS_MANAGE)
        .await?;
    let items: Vec<CredentialDto> = state
        .service
        .list_for_user(user.user_id())
        .map_err(|e| CredentialError::Internal(e.to_string()))?
        .into_iter()
        .map(Into::into)
        .collect();
    audit_event(
        &state,
        user.user_id(),
        "credentials.list",
        "*",
        AuditOutcome::Success,
    )
    .await;
    Ok(Json(ListCredentialsResponse { items }))
}

/// `DELETE /credentials/{provider}` — révoque un credential.
#[utoipa::path(
    delete,
    path = "/credentials/{provider}",
    tag = "credentials",
    params(("provider" = String, Path, description = "Identifiant du provider OAuth")),
    responses(
        (status = 204, description = "Révoqué (idempotent)"),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `credentials.manage`")
    ),
    security(("bearer" = []))
)]
pub async fn delete_credential(
    user: AuthenticatedUser,
    State(state): State<CredentialsState>,
    Path(provider_id): Path<String>,
) -> Result<StatusCode, CredentialError> {
    state
        .auth
        .require_permission(&user, PERM_CREDENTIALS_MANAGE)
        .await?;
    state
        .service
        .delete(user.user_id(), &provider_id)
        .map_err(|e| CredentialError::Internal(e.to_string()))?;
    audit_event(
        &state,
        user.user_id(),
        "credentials.delete",
        &provider_id,
        AuditOutcome::Success,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Construit le sous-router credentials.
pub fn credentials_router(state: CredentialsState) -> Router {
    Router::new()
        .route("/credentials/providers", get(list_providers))
        .route("/credentials/:provider/authorize", post(start_authorize))
        .route("/credentials/:provider/callback", post(complete_authorize))
        .route("/credentials", get(list_credentials))
        .route("/credentials/:provider", delete(delete_credential))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_connectors::oauth::client::AuthorizationStart;
    use autome_connectors::{CredentialSummary, ProviderInfo};
    use autome_core::Result as CoreResult;
    use autome_core::{
        AuditSink, InMemoryAuditSink, InMemoryPermissionStore, InMemoryRevocationStore,
        PermissionStore, RevocationStore, Role, TokenService,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const FIXED_NOW: i64 = 1_700_000_000;

    struct FixedClock;
    impl crate::auth::Clock for FixedClock {
        fn now_unix_seconds(&self) -> i64 {
            FIXED_NOW
        }
    }

    /// Mock thread-safe — simule un `CredentialService` sans HTTP réel.
    struct FakeService {
        providers: Vec<ProviderInfo>,
        // (user_id, provider_id) -> CredentialSummary
        stored: Mutex<std::collections::BTreeMap<(String, String), CredentialSummary>>,
        // configure le résultat du prochain start_authorize
        authorize_result: Mutex<Option<CoreResult<AuthorizationStart>>>,
        // configure le résultat du prochain complete_authorize
        complete_result: Mutex<Option<CoreResult<CredentialSummary>>>,
    }

    impl FakeService {
        fn new() -> Self {
            Self {
                providers: vec![
                    ProviderInfo {
                        provider_id: "google".into(),
                        scopes: vec!["openid".into(), "email".into()],
                    },
                    ProviderInfo {
                        provider_id: "slack".into(),
                        scopes: vec!["chat:write".into()],
                    },
                ],
                stored: Mutex::new(Default::default()),
                authorize_result: Mutex::new(None),
                complete_result: Mutex::new(None),
            }
        }

        fn set_authorize(&self, r: CoreResult<AuthorizationStart>) {
            *self.authorize_result.lock().unwrap() = Some(r);
        }

        fn set_complete(&self, r: CoreResult<CredentialSummary>) {
            *self.complete_result.lock().unwrap() = Some(r);
        }
    }

    impl CredentialService for FakeService {
        fn list_providers(&self) -> Vec<ProviderInfo> {
            self.providers.clone()
        }

        fn start_authorize(&self, provider_id: &str) -> CoreResult<AuthorizationStart> {
            if let Some(r) = self.authorize_result.lock().unwrap().take() {
                return r;
            }
            // Défaut : URL synthétique pour ce provider.
            Ok(AuthorizationStart {
                authorize_url: format!("https://example.test/{provider_id}/auth"),
                state: "state-abc".into(),
                code_verifier: "verifier-xyz".into(),
            })
        }

        fn complete_authorize(
            &self,
            user_id: &str,
            provider_id: &str,
            _code: &str,
            _code_verifier: &str,
            _now_ms: i64,
        ) -> CoreResult<CredentialSummary> {
            if let Some(r) = self.complete_result.lock().unwrap().take() {
                return r;
            }
            let summary = CredentialSummary {
                provider_id: provider_id.into(),
                has_refresh_token: true,
                scope: Some("openid".into()),
                expires_at_ms: Some(2_000_000_000_000),
            };
            self.stored
                .lock()
                .unwrap()
                .insert((user_id.into(), provider_id.into()), summary.clone());
            Ok(summary)
        }

        fn list_for_user(&self, user_id: &str) -> CoreResult<Vec<CredentialSummary>> {
            let map = self.stored.lock().unwrap();
            let mut out: Vec<CredentialSummary> = map
                .iter()
                .filter(|((u, _), _)| u == user_id)
                .map(|(_, v)| v.clone())
                .collect();
            out.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
            Ok(out)
        }

        fn delete(&self, user_id: &str, provider_id: &str) -> CoreResult<()> {
            self.stored
                .lock()
                .unwrap()
                .remove(&(user_id.into(), provider_id.into()));
            Ok(())
        }
    }

    fn make_test_state() -> (
        CredentialsState,
        TokenService,
        Arc<InMemoryPermissionStore>,
        Arc<InMemoryAuditSink>,
        Arc<FakeService>,
    ) {
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let perms = Arc::new(InMemoryPermissionStore::new());
        let audit = Arc::new(InMemoryAuditSink::new());
        let svc = TokenService::new(SECRET);
        let fake = Arc::new(FakeService::new());
        let auth = AuthState::new(
            TokenService::new(SECRET),
            revocation,
            Arc::clone(&perms) as Arc<dyn PermissionStore + Send + Sync>,
            Arc::clone(&audit) as Arc<dyn AuditSink + Send + Sync>,
        )
        .with_clock(Arc::new(FixedClock));
        let state = CredentialsState::new(
            auth,
            Arc::clone(&fake) as Arc<dyn CredentialService + Send + Sync>,
        );
        (state, svc, perms, audit, fake)
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

    /// Rôle qui a `credentials.manage` (super_admin par défaut le porte).
    fn admin_role() -> Role {
        Role::super_admin()
    }

    // ───── F7.3.b — GET /credentials/providers ─────

    #[tokio::test]
    async fn list_providers_returns_items_sorted_without_secrets() {
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req("GET", "/credentials/providers", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let items = parsed["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["provider_id"], "google");
        assert_eq!(items[1]["provider_id"], "slack");
        assert!(!body.contains("client_secret"));
        assert!(!body.contains("secret-"));
    }

    // ───── F7.3.b — POST /credentials/{provider}/authorize ─────

    #[tokio::test]
    async fn start_authorize_returns_url_state_and_verifier() {
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req(
                "POST",
                "/credentials/google/authorize",
                &token,
                Some("{}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["authorize_url"], "https://example.test/google/auth");
        assert_eq!(body["state"], "state-abc");
        assert_eq!(body["code_verifier"], "verifier-xyz");
    }

    #[tokio::test]
    async fn start_authorize_returns_404_when_provider_unknown() {
        let (state, svc, perms, _, fake) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        fake.set_authorize(Err(autome_core::Error::Invalid(
            "provider 'ghost' inconnu".into(),
        )));
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req(
                "POST",
                "/credentials/ghost/authorize",
                &token,
                Some("{}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ───── F7.3.b — POST /credentials/{provider}/callback ─────

    #[tokio::test]
    async fn callback_persists_credentials_for_current_user_and_returns_summary() {
        let (state, svc, perms, _, fake) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req(
                "POST",
                "/credentials/google/callback",
                &token,
                Some(r#"{"code":"c","state":"state-abc","code_verifier":"verifier-xyz"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["provider_id"], "google");
        assert_eq!(body["has_refresh_token"], true);
        assert!(!body.to_string().contains("access_token"));
        // Le credential est persisté pour alice.
        let stored = fake.list_for_user("alice").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].provider_id, "google");
    }

    #[tokio::test]
    async fn callback_returns_502_when_upstream_fails() {
        let (state, svc, perms, _, fake) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        fake.set_complete(Err(autome_core::Error::Invalid(
            "oauth exchange : http 401".into(),
        )));
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req(
                "POST",
                "/credentials/google/callback",
                &token,
                Some(r#"{"code":"bad","state":"s","code_verifier":"v"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn callback_returns_400_when_body_missing_fields() {
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req(
                "POST",
                "/credentials/google/callback",
                &token,
                Some(r#"{"code":"c"}"#),
            ))
            .await
            .unwrap();
        // Axum renvoie 422 quand le body JSON est mal formé pour le DTO.
        assert!(matches!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST
        ));
    }

    // ───── F7.3.b — GET /credentials & DELETE /credentials/{provider} ─────

    #[tokio::test]
    async fn list_credentials_returns_only_current_user_entries() {
        let (state, svc, perms, _, fake) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        perms.assign_role("bob", &admin_role()).unwrap();
        // Pré-état : alice a slack, bob a google.
        fake.complete_authorize("alice", "slack", "c", "v", 0)
            .unwrap();
        fake.complete_authorize("bob", "google", "c", "v", 0)
            .unwrap();
        let token_alice = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req("GET", "/credentials", &token_alice, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["provider_id"], "slack");
    }

    #[tokio::test]
    async fn delete_credential_returns_204_and_removes_entry() {
        let (state, svc, perms, _, fake) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        fake.complete_authorize("alice", "google", "c", "v", 0)
            .unwrap();
        assert_eq!(fake.list_for_user("alice").unwrap().len(), 1);
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req("DELETE", "/credentials/google", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(fake.list_for_user("alice").unwrap().is_empty());
    }

    // ───── F7.3.c — RBAC ─────

    #[tokio::test]
    async fn list_providers_returns_401_without_auth() {
        let (state, _, _, _, _) = make_test_state();
        let app = credentials_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/credentials/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn start_authorize_returns_403_for_user_without_credentials_manage() {
        // Viewer ne porte pas `credentials.manage` (editor le porte, lui).
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req(
                "POST",
                "/credentials/google/authorize",
                &token,
                Some("{}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_credential_returns_403_without_permission() {
        let (state, svc, perms, _, _) = make_test_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let response = app
            .oneshot(auth_req("DELETE", "/credentials/google", &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ───── F7.3.c — Audit ─────

    #[tokio::test]
    async fn authorize_records_audit_event_with_provider_id() {
        let (state, svc, perms, audit, _) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let _ = app
            .oneshot(auth_req(
                "POST",
                "/credentials/google/authorize",
                &token,
                Some("{}"),
            ))
            .await
            .unwrap();
        let events = audit.list().await.unwrap();
        let evt = events
            .iter()
            .find(|e| e.action == "credentials.authorize" && e.outcome == AuditOutcome::Success)
            .expect("credentials.authorize success event présent");
        assert_eq!(evt.actor, "alice");
        assert_eq!(evt.resource.as_deref(), Some("google"));
    }

    #[tokio::test]
    async fn callback_records_failure_audit_event_when_upstream_fails() {
        let (state, svc, perms, audit, fake) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        fake.set_complete(Err(autome_core::Error::Invalid("oauth http 401".into())));
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let _ = app
            .oneshot(auth_req(
                "POST",
                "/credentials/google/callback",
                &token,
                Some(r#"{"code":"c","state":"s","code_verifier":"v"}"#),
            ))
            .await
            .unwrap();
        let evt = audit
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.action == "credentials.callback" && e.outcome == AuditOutcome::Failure)
            .expect("failure event présent");
        assert_eq!(evt.resource.as_deref(), Some("google"));
    }

    #[tokio::test]
    async fn delete_records_audit_event() {
        let (state, svc, perms, audit, _) = make_test_state();
        perms.assign_role("alice", &admin_role()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = credentials_router(state);
        let _ = app
            .oneshot(auth_req("DELETE", "/credentials/google", &token, None))
            .await
            .unwrap();
        let evt = audit
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.action == "credentials.delete")
            .expect("credentials.delete event");
        assert_eq!(evt.resource.as_deref(), Some("google"));
    }
}

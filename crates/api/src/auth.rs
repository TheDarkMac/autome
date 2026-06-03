//! Middleware d'authentification axum (F6.1.c).
//!
//! Expose un extractor [`AuthenticatedUser`] qui lit le header
//! `Authorization: Bearer <jwt>` et :
//! 1. vérifie la signature + expiration via [`TokenService`] ;
//! 2. contrôle la révocation via [`RevocationStore`] ;
//! 3. retourne `401 Unauthorized` typé en cas d'échec.
//!
//! L'état (`AuthState`) est attaché au router via `with_state`. L'horloge
//! est **injectable** ([`Clock`]) pour rendre les tests déterministes —
//! l'impl par défaut [`SystemClock`] lit `SystemTime::now()`.
//!
//! ```ignore
//! use autome_core::{InMemoryRevocationStore, TokenService};
//! use autome_api::auth::{protected_router, AuthState};
//! use std::sync::Arc;
//!
//! let state = AuthState::new(
//!     TokenService::new(b"shared-secret-at-least-32-bytes!!"),
//!     Arc::new(InMemoryRevocationStore::new()),
//! );
//! let app = protected_router(state);
//! ```

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use autome_core::{
    has_permission, AuditEvent, AuditOutcome, AuditSink, Claims, Config, InMemoryPermissionStore,
    InMemoryRevocationStore, NoopAuditSink, PermissionStore, RevocationStore, TokenService,
};
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde_json::json;

/// Horloge injectable (seconds Unix). Permet aux tests d'épingler `now`.
pub trait Clock: Send + Sync {
    /// Renvoie le temps courant en secondes depuis l'epoch Unix.
    fn now_unix_seconds(&self) -> i64;
}

/// Impl basée sur `std::time::SystemTime` (production).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// État partagé du middleware d'auth (à fournir à `Router::with_state`).
#[derive(Clone)]
pub struct AuthState {
    /// Service d'émission/vérification JWT.
    pub token_service: Arc<TokenService>,
    /// Store de révocation par `jti`.
    pub revocation: Arc<dyn RevocationStore + Send + Sync>,
    /// Store de permissions par `user_id` (F6.5).
    pub permissions: Arc<dyn PermissionStore + Send + Sync>,
    /// Sink d'audit pour les décisions d'autorisation (F6.6). Toujours
    /// présent — passer `Arc::new(NoopAuditSink)` pour désactiver.
    pub audit: Arc<dyn AuditSink + Send + Sync>,
    /// Horloge injectable (par défaut `SystemClock`).
    pub clock: Arc<dyn Clock>,
}

/// Dérive un [`TokenService`] du secret JWT de la configuration
/// (`config.jwt_secret`). Point d'usage unique du secret en clair.
pub fn token_service_from_config(config: &Config) -> TokenService {
    TokenService::new(config.jwt_secret.expose().as_bytes())
}

impl AuthState {
    /// Construit l'état avec [`SystemClock`] et un store de permissions vide.
    pub fn new(
        token_service: TokenService,
        revocation: Arc<dyn RevocationStore + Send + Sync>,
        permissions: Arc<dyn PermissionStore + Send + Sync>,
        audit: Arc<dyn AuditSink + Send + Sync>,
    ) -> Self {
        Self {
            token_service: Arc::new(token_service),
            revocation,
            permissions,
            audit,
            clock: Arc::new(SystemClock),
        }
    }

    /// Builder : remplace l'horloge (utile pour les tests).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Construit l'`AuthState` à partir de la configuration applicative
    /// (dette #107, constat 1) : le `TokenService` est dérivé de
    /// **`config.jwt_secret`** — fini le secret codé en dur dans `main`.
    ///
    /// Les stores de révocation/permissions et le sink d'audit sont encore
    /// **in-memory** (non persistés, non partagés entre répliques) : leur
    /// adossement à PostgreSQL relève de la suite de #107 (constat 2).
    pub fn from_config(config: &Config) -> Self {
        Self::new(
            token_service_from_config(config),
            Arc::new(InMemoryRevocationStore::new()),
            Arc::new(InMemoryPermissionStore::new()),
            Arc::new(NoopAuditSink),
        )
    }

    /// Vérifie que `user` possède la permission `needed`. À appeler depuis
    /// un handler axum après extraction de [`AuthenticatedUser`] (F6.5.b).
    ///
    /// Enregistre **systématiquement** un event audit
    /// `"auth.permission_check"` (F6.6) :
    /// - actor = `user_id`,
    /// - resource = `needed` (la permission demandée),
    /// - outcome = `Success` ou `Denied` (ou `Failure` si erreur interne).
    ///
    /// ```ignore
    /// async fn admin_only(user: AuthenticatedUser, State(state): State<AuthState>) -> Result<String, AuthError> {
    ///     state.require_permission(&user, autome_core::PERM_USERS_ADMIN)?;
    ///     Ok(format!("hi {}", user.user_id()))
    /// }
    /// ```
    pub async fn require_permission(
        &self,
        user: &AuthenticatedUser,
        needed: &str,
    ) -> Result<(), AuthError> {
        let now_ms = self.clock.now_unix_seconds().saturating_mul(1_000);
        let perms = match self.permissions.permissions_for(user.user_id()).await {
            Ok(p) => p,
            Err(e) => {
                let _ = self
                    .audit
                    .record(
                        AuditEvent::new(
                            now_ms,
                            user.user_id(),
                            "auth.permission_check",
                            AuditOutcome::Failure,
                        )
                        .with_resource(needed)
                        .with_metadata(json!({ "error": e.to_string() })),
                    )
                    .await;
                return Err(AuthError::Internal(e.to_string()));
            }
        };
        let allowed = has_permission(&perms, needed);
        let outcome = if allowed {
            AuditOutcome::Success
        } else {
            AuditOutcome::Denied
        };
        // L'erreur d'audit n'invalide pas la décision : si le sink échoue
        // on log côté stderr et on continue (refuser un accès légitime à
        // cause d'un audit cassé serait pire que de manquer un event).
        if let Err(e) = self
            .audit
            .record(
                AuditEvent::new(now_ms, user.user_id(), "auth.permission_check", outcome)
                    .with_resource(needed),
            )
            .await
        {
            eprintln!("[auth] audit record failed: {e}");
        }
        if allowed {
            Ok(())
        } else {
            Err(AuthError::Forbidden(needed.to_string()))
        }
    }
}

/// Causes possibles de rejet d'un appel authentifié.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Header `Authorization` absent.
    Missing,
    /// Header présent mais mal formé (pas `Bearer …`, ASCII non valide).
    Malformed,
    /// Token rejeté à la vérification (signature, expiration, …).
    Invalid(String),
    /// Token révoqué (jti dans `RevocationStore`).
    Revoked,
    /// User authentifié, mais permission manquante (F6.5) — porte la
    /// permission attendue pour faciliter le debug côté UI.
    Forbidden(String),
    /// Erreur interne (lock empoisonné, etc.). Plus rare.
    Internal(String),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AuthError::Missing => (
                StatusCode::UNAUTHORIZED,
                "missing authorization".to_string(),
            ),
            AuthError::Malformed => (
                StatusCode::UNAUTHORIZED,
                "malformed authorization".to_string(),
            ),
            AuthError::Invalid(_) => (StatusCode::UNAUTHORIZED, "invalid token".to_string()),
            AuthError::Revoked => (StatusCode::UNAUTHORIZED, "token revoked".to_string()),
            AuthError::Forbidden(perm) => (
                StatusCode::FORBIDDEN,
                format!("forbidden: missing '{perm}'"),
            ),
            AuthError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "auth internal error".to_string(),
            ),
        };
        (status, msg).into_response()
    }
}

/// Utilisateur authentifié extrait d'une requête HTTP.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    /// Claims décodées du JWT (sub, exp, iat, jti, kind).
    pub claims: Claims,
}

impl AuthenticatedUser {
    /// Raccourci : identifiant de l'utilisateur (`claims.sub`).
    pub fn user_id(&self) -> &str {
        &self.claims.sub
    }
}

#[axum::async_trait]
impl<S> FromRequestParts<S> for AuthenticatedUser
where
    AuthState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = AuthState::from_ref(state);
        let header = parts
            .headers
            .get("Authorization")
            .ok_or(AuthError::Missing)?
            .to_str()
            .map_err(|_| AuthError::Malformed)?;
        let token = header.strip_prefix("Bearer ").ok_or(AuthError::Malformed)?;

        let now = auth.clock.now_unix_seconds();
        let claims = auth
            .token_service
            .verify(token, now)
            .map_err(|e| AuthError::Invalid(e.to_string()))?;

        let revoked = auth
            .revocation
            .is_revoked(&claims.jti)
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))?;
        if revoked {
            return Err(AuthError::Revoked);
        }

        Ok(AuthenticatedUser { claims })
    }
}

/// Handler protégé de démonstration : renvoie `hello <sub>` à l'utilisateur
/// authentifié. Utilisé par les tests d'intégration ; remplacable par
/// les vrais handlers métier (F7).
pub async fn whoami(user: AuthenticatedUser) -> String {
    format!("hello {}", user.user_id())
}

/// Handler admin de démonstration (F6.5.b) : exige la permission
/// [`autome_core::PERM_USERS_ADMIN`].
pub async fn admin_only(
    user: AuthenticatedUser,
    axum::extract::State(state): axum::extract::State<AuthState>,
) -> Result<String, AuthError> {
    state
        .require_permission(&user, autome_core::PERM_USERS_ADMIN)
        .await?;
    Ok(format!("admin ok for {}", user.user_id()))
}

/// Construit un sous-router protégé sur `/whoami` (auth seul) + `/admin`
/// (auth + permission `users.admin`). À assembler avec le router racine
/// via `Router::nest` côté `main`.
pub fn protected_router(state: AuthState) -> Router {
    Router::new()
        .route("/whoami", get(whoami))
        .route("/admin", get(admin_only))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{
        InMemoryAuditSink, InMemoryPermissionStore, InMemoryRevocationStore, Role, TokenService,
        PERM_USERS_ADMIN,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const FIXED_NOW: i64 = 1_700_000_000;

    /// Horloge épinglée à une valeur arbitraire — déterminisme des tests.
    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now_unix_seconds(&self) -> i64 {
            self.0
        }
    }

    /// Horloge ajustable (pour simuler `now` qui avance).
    #[derive(Default)]
    struct MovableClock(Mutex<i64>);
    impl MovableClock {
        fn new(start: i64) -> Self {
            Self(Mutex::new(start))
        }
        fn advance(&self, secs: i64) {
            *self.0.lock().unwrap() += secs;
        }
    }
    impl Clock for MovableClock {
        fn now_unix_seconds(&self) -> i64 {
            *self.0.lock().unwrap()
        }
    }

    fn make_state() -> (
        AuthState,
        TokenService,
        Arc<InMemoryPermissionStore>,
        Arc<InMemoryAuditSink>,
    ) {
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let perms = Arc::new(InMemoryPermissionStore::new());
        let audit = Arc::new(InMemoryAuditSink::new());
        let svc = TokenService::new(SECRET);
        let state = AuthState::new(
            TokenService::new(SECRET),
            revocation,
            Arc::clone(&perms) as Arc<dyn PermissionStore + Send + Sync>,
            Arc::clone(&audit) as Arc<dyn AuditSink + Send + Sync>,
        )
        .with_clock(Arc::new(FixedClock(FIXED_NOW)));
        (state, svc, perms, audit)
    }

    async fn body_string(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn req_with_auth(token: &str) -> Request<Body> {
        Request::builder()
            .uri("/whoami")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    // ───── chemin nominal ─────

    #[tokio::test]
    async fn valid_token_returns_200_with_user_id() {
        let (state, svc, _, _) = make_state();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        let response = app.oneshot(req_with_auth(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "hello alice");
    }

    // ───── rejets ─────

    #[tokio::test]
    async fn missing_authorization_returns_401() {
        let (state, _, _, _) = make_state();
        let app = protected_router(state);
        let req = Request::builder()
            .uri("/whoami")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_string(response).await, "missing authorization");
    }

    #[tokio::test]
    async fn malformed_header_returns_401() {
        let (state, _, _, _) = make_state();
        let app = protected_router(state);
        let req = Request::builder()
            .uri("/whoami")
            .header("Authorization", "Basic dXNlcjpwYXNz")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_string(response).await, "malformed authorization");
    }

    #[tokio::test]
    async fn expired_token_returns_401_invalid() {
        // Horloge mobile : émet à FIXED_NOW, avance au-delà de exp avant la requête.
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let perms: Arc<dyn PermissionStore + Send + Sync> =
            Arc::new(InMemoryPermissionStore::new());
        let audit: Arc<dyn AuditSink + Send + Sync> = Arc::new(InMemoryAuditSink::new());
        let clock = Arc::new(MovableClock::new(FIXED_NOW));
        let svc = TokenService::new(SECRET);
        let state = AuthState::new(TokenService::new(SECRET), revocation, perms, audit)
            .with_clock(clock.clone());
        let token = svc.issue_access("u", 60, FIXED_NOW).unwrap();

        // Avance dans le futur, au-delà de l'expiration
        clock.advance(61);

        let app = protected_router(state);
        let response = app.oneshot(req_with_auth(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_string(response).await, "invalid token");
    }

    #[tokio::test]
    async fn tampered_token_returns_401_invalid() {
        let (state, svc, _, _) = make_state();
        let mut token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        token.pop();
        token.push('X');
        let app = protected_router(state);
        let response = app.oneshot(req_with_auth(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_string(response).await, "invalid token");
    }

    #[tokio::test]
    async fn revoked_token_returns_401_revoked() {
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let perms: Arc<dyn PermissionStore + Send + Sync> =
            Arc::new(InMemoryPermissionStore::new());
        let audit: Arc<dyn AuditSink + Send + Sync> = Arc::new(InMemoryAuditSink::new());
        let svc = TokenService::new(SECRET);
        let token = svc.issue_access("u", 900, FIXED_NOW).unwrap();
        // Récupère le jti pour le révoquer manuellement
        let claims = svc.verify(&token, FIXED_NOW + 1).unwrap();
        revocation
            .revoke(&claims.jti, (claims.exp + 100) * 1_000)
            .await
            .unwrap();

        let state = AuthState::new(TokenService::new(SECRET), revocation, perms, audit)
            .with_clock(Arc::new(FixedClock(FIXED_NOW)));
        let app = protected_router(state);
        let response = app.oneshot(req_with_auth(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_string(response).await, "token revoked");
    }

    // ───── F6.5.b — Enforcement permission ─────

    fn admin_req(token: &str) -> Request<Body> {
        Request::builder()
            .uri("/admin")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn admin_route_requires_users_admin_permission() {
        let (state, svc, perms, _) = make_state();
        perms.assign_role("alice", &Role::super_admin()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        let response = app.oneshot(admin_req(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "admin ok for alice");
    }

    #[tokio::test]
    async fn admin_route_returns_403_when_permission_missing() {
        let (state, svc, perms, _) = make_state();
        // alice est viewer seul (pas d'admin)
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        let response = app.oneshot(admin_req(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_string(response).await;
        assert!(body.contains("forbidden"), "body={body}");
        assert!(body.contains(PERM_USERS_ADMIN), "body={body}");
    }

    #[tokio::test]
    async fn admin_route_returns_403_when_no_permissions_at_all() {
        let (state, svc, _, _) = make_state();
        // Aucun rôle attribué → set vide
        let token = svc.issue_access("bob", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        let response = app.oneshot(admin_req(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_route_returns_401_when_token_missing_before_perm_check() {
        // Sans token → 401 (la chaîne s'arrête avant le check permission)
        let (state, _, _, _) = make_state();
        let app = protected_router(state);
        let req = Request::builder()
            .uri("/admin")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_route_succeeds_with_exact_permission_not_role() {
        // Pas de rôle, juste la permission individuelle
        let (state, svc, perms, _) = make_state();
        perms.grant("carol", PERM_USERS_ADMIN).unwrap();
        let token = svc.issue_access("carol", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        let response = app.oneshot(admin_req(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn user_id_helper_returns_sub() {
        let claims = Claims {
            sub: "bob".into(),
            iat: 0,
            exp: 100,
            jti: "j".into(),
            kind: autome_core::TokenKind::Access,
        };
        let user = AuthenticatedUser { claims };
        assert_eq!(user.user_id(), "bob");
    }

    // ───── F6.6 — Audit log ─────

    #[tokio::test]
    async fn permission_grant_records_success_audit_event() {
        let (state, svc, perms, audit) = make_state();
        perms.assign_role("alice", &Role::super_admin()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        let _ = app.oneshot(admin_req(&token)).await.unwrap();
        let events = audit.list().await.unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.actor, "alice");
        assert_eq!(e.action, "auth.permission_check");
        assert_eq!(e.outcome, AuditOutcome::Success);
        assert_eq!(e.resource.as_deref(), Some(PERM_USERS_ADMIN));
        assert_eq!(e.timestamp_ms, FIXED_NOW * 1000);
    }

    #[tokio::test]
    async fn permission_denial_records_denied_audit_event() {
        let (state, svc, perms, audit) = make_state();
        perms.assign_role("alice", &Role::viewer()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        let _ = app.oneshot(admin_req(&token)).await.unwrap();
        let events = audit.list().await.unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.actor, "alice");
        assert_eq!(e.outcome, AuditOutcome::Denied);
        assert_eq!(e.resource.as_deref(), Some(PERM_USERS_ADMIN));
    }

    #[tokio::test]
    async fn unauthenticated_calls_record_no_audit_event() {
        // 401 (token absent) → on n'arrive jamais à require_permission,
        // donc aucun event audité.
        let (state, _, _, audit) = make_state();
        let app = protected_router(state);
        let req = Request::builder()
            .uri("/admin")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert!(audit.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn multiple_calls_accumulate_audit_events_in_order() {
        let (state, svc, perms, audit) = make_state();
        perms.assign_role("alice", &Role::super_admin()).unwrap();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        for _ in 0..3 {
            let _ = app.clone().oneshot(admin_req(&token)).await.unwrap();
        }
        let events = audit.list().await.unwrap();
        assert_eq!(events.len(), 3);
        for e in &events {
            assert_eq!(e.actor, "alice");
            assert_eq!(e.outcome, AuditOutcome::Success);
        }
    }

    // ── Dette #107 — le secret JWT vient bien de la config ─────────────────

    /// Constat 1 : `token_service_from_config` signe avec `config.jwt_secret`.
    /// Un token émis avec ce service se vérifie ; un service construit avec
    /// l'**ancienne constante codée en dur** rejette ce même token.
    #[test]
    fn token_service_uses_config_jwt_secret() {
        let config = Config::load(|k| match k {
            "JWT_SECRET" => Some("a-very-real-production-secret-32+chars".into()),
            _ => None,
        })
        .expect("config");

        let svc = token_service_from_config(&config);
        let token = svc.issue_access("alice", 3600, FIXED_NOW).unwrap();
        // Le service issu de la config se vérifie lui-même.
        assert!(svc.verify(&token, FIXED_NOW).is_ok());

        // L'ancien secret de dev codé en dur ne doit PAS valider ce token.
        let legacy = TokenService::new(b"DEV-ONLY-replace-with-config-secret-please-32");
        assert!(
            legacy.verify(&token, FIXED_NOW).is_err(),
            "régression : le token ne doit pas être validé par l'ancien secret codé en dur"
        );
    }

    /// `AuthState::from_config` produit un état dont le `token_service` utilise
    /// le secret de configuration (cohérent avec `token_service_from_config`).
    #[test]
    fn auth_state_from_config_wires_token_service() {
        let config = Config::load(|k| match k {
            "JWT_SECRET" => Some("another-distinct-real-secret-32+chars".into()),
            _ => None,
        })
        .expect("config");

        let state = AuthState::from_config(&config);
        let token = state
            .token_service
            .issue_access("bob", 3600, FIXED_NOW)
            .unwrap();
        assert!(state.token_service.verify(&token, FIXED_NOW).is_ok());

        let other = TokenService::new(b"some-other-unrelated-secret-32+chars!");
        assert!(other.verify(&token, FIXED_NOW).is_err());
    }
}

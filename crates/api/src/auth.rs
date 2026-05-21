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

use autome_core::{Claims, RevocationStore, TokenService};
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

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
    /// Horloge injectable (par défaut `SystemClock`).
    pub clock: Arc<dyn Clock>,
}

impl AuthState {
    /// Construit l'état avec [`SystemClock`].
    pub fn new(
        token_service: TokenService,
        revocation: Arc<dyn RevocationStore + Send + Sync>,
    ) -> Self {
        Self {
            token_service: Arc::new(token_service),
            revocation,
            clock: Arc::new(SystemClock),
        }
    }

    /// Builder : remplace l'horloge (utile pour les tests).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
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
    /// Erreur interne (lock empoisonné, etc.). Plus rare.
    Internal(String),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AuthError::Missing => (StatusCode::UNAUTHORIZED, "missing authorization"),
            AuthError::Malformed => (StatusCode::UNAUTHORIZED, "malformed authorization"),
            AuthError::Invalid(_) => (StatusCode::UNAUTHORIZED, "invalid token"),
            AuthError::Revoked => (StatusCode::UNAUTHORIZED, "token revoked"),
            AuthError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "auth internal error"),
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
impl FromRequestParts<AuthState> for AuthenticatedUser {
    type Rejection = AuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AuthState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get("Authorization")
            .ok_or(AuthError::Missing)?
            .to_str()
            .map_err(|_| AuthError::Malformed)?;
        let token = header.strip_prefix("Bearer ").ok_or(AuthError::Malformed)?;

        let now = state.clock.now_unix_seconds();
        let claims = state
            .token_service
            .verify(token, now)
            .map_err(|e| AuthError::Invalid(e.to_string()))?;

        let revoked = state
            .revocation
            .is_revoked(&claims.jti)
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

/// Construit un sous-router protégé sur `/whoami` (démo). À assembler avec
/// le router racine via `Router::nest` côté `main`.
pub fn protected_router(state: AuthState) -> Router {
    Router::new()
        .route("/whoami", get(whoami))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{InMemoryRevocationStore, TokenService};
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

    fn make_state() -> (AuthState, TokenService) {
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let svc = TokenService::new(SECRET);
        let state = AuthState::new(TokenService::new(SECRET), revocation)
            .with_clock(Arc::new(FixedClock(FIXED_NOW)));
        (state, svc)
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
        let (state, svc) = make_state();
        let token = svc.issue_access("alice", 900, FIXED_NOW).unwrap();
        let app = protected_router(state);
        let response = app.oneshot(req_with_auth(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "hello alice");
    }

    // ───── rejets ─────

    #[tokio::test]
    async fn missing_authorization_returns_401() {
        let (state, _) = make_state();
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
        let (state, _) = make_state();
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
        let clock = Arc::new(MovableClock::new(FIXED_NOW));
        let svc = TokenService::new(SECRET);
        let state = AuthState::new(TokenService::new(SECRET), revocation).with_clock(clock.clone());
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
        let (state, svc) = make_state();
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
        let svc = TokenService::new(SECRET);
        let token = svc.issue_access("u", 900, FIXED_NOW).unwrap();
        // Récupère le jti pour le révoquer manuellement
        let claims = svc.verify(&token, FIXED_NOW + 1).unwrap();
        revocation
            .revoke(&claims.jti, (claims.exp + 100) * 1_000)
            .unwrap();

        let state = AuthState::new(TokenService::new(SECRET), revocation)
            .with_clock(Arc::new(FixedClock(FIXED_NOW)));
        let app = protected_router(state);
        let response = app.oneshot(req_with_auth(&token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_string(response).await, "token revoked");
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
}

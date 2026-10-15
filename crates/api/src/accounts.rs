//! Cycle de vie des comptes : inscription, connexion, rotation des jetons
//! (backlog post-audit — auth P0).
//!
//! Endpoints REST/JSON, **publics** (ils émettent les jetons) :
//!
//! | Méthode | Path            | Body                    | Succès                      |
//! |---------|-----------------|-------------------------|-----------------------------|
//! | POST    | `/auth/signup`  | `{ email, password }`   | 201 + [`TokenResponse`]     |
//! | POST    | `/auth/login`   | `{ email, password }`   | 200 + [`TokenResponse`]     |
//! | POST    | `/auth/refresh` | `{ refresh_token }`     | 200 + [`TokenResponse`]     |
//! | POST    | `/auth/logout`  | `{ refresh_token }`     | 204                         |
//! | POST    | `/auth/logout-all` | — (Bearer requis)    | 204                         |
//!
//! Contrat aligné sur le frontend (`frontend/src/auth.ts`) : 400 saisie
//! invalide, 401 identifiants/jeton refusés, 409 email déjà pris, 429 trop
//! d'échecs de connexion.
//!
//! Choix de sécurité :
//! - **Argon2id** ([`PasswordHasher`]) exécuté dans `spawn_blocking` : ~50 ms
//!   de CPU par hash ne doivent pas bloquer le runtime async.
//! - **Pas d'énumération de comptes** au login : email inconnu et mauvais mot
//!   de passe donnent la même réponse 401, et un email inconnu paie quand même
//!   une vérification Argon2 (contre un hash leurre) pour égaliser le temps.
//! - **Anti-bruteforce** ([`LoginThrottleStore`]) : au-delà de
//!   [`MAX_LOGIN_FAILURES`] tentatives sans succès sur un email dans la
//!   fenêtre, 429 jusqu'à sa fin. En mode `postgres`, le compteur est
//!   **partagé** par toutes les répliques et survit aux redémarrages (#182) ;
//!   une erreur du compteur refuse la connexion (fail closed).
//! - Le refresh réutilise [`autome_core::TokenService::rotate`] : le refresh
//!   présenté est révoqué, un rejeu est refusé — y compris entre requêtes
//!   concurrentes (révocation atomique `revoke_once`, #184).
//! - Le logout (#184) révoque côté serveur le refresh présenté et, s'il est
//!   fourni en `Authorization: Bearer`, l'access token de la même session :
//!   vider le stockage du client ne suffit pas, un jeton copié resterait
//!   valide jusqu'à son expiration.
//! - Le logout-all (#186) ferme **toutes** les sessions de l'utilisateur, sur
//!   tous les appareils : il incrémente sa version de session, que portent
//!   tous ses jetons (claim `sv`) ; ceux émis avant sont refusés partout,
//!   access comme refresh. Les jetons sont toujours émis à la version
//!   courante.
//! - Chaque inscription/connexion/déconnexion produit un event d'audit
//!   `auth.signup` / `auth.login` / `auth.logout` / `auth.logout_all`.

use std::collections::HashSet;
use std::sync::Arc;

use autome_core::{
    normalize_email, validate_email, validate_password, AuditEvent, AuditOutcome, CreateUser,
    InMemoryLoginThrottle, LoginAttempt, LoginThrottleStore, PasswordHasher, Role, TokenKind,
    UserStore,
};
pub use autome_core::{LOGIN_WINDOW_SECONDS, MAX_LOGIN_FAILURES};
use axum::{
    extract::{FromRef, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{bearer_token, AuthState, AuthenticatedUser};

/// Durée de vie d'un access token (15 min).
pub const ACCESS_TTL_SECONDS: i64 = 15 * 60;
/// Durée de vie d'un refresh token (7 jours).
pub const REFRESH_TTL_SECONDS: i64 = 7 * 24 * 3600;
/// Mot de passe du hash leurre (jamais attribué à un compte).
const DUMMY_PASSWORD: &str = "autome-dummy-password-for-timing";

/// État des handlers de comptes.
#[derive(Clone)]
pub struct AccountsState {
    /// État d'authentification (émission/rotation des jetons, audit, horloge).
    pub auth: AuthState,
    /// Persistance des comptes.
    pub users: Arc<dyn UserStore + Send + Sync>,
    /// Hash/vérification des mots de passe.
    pub hasher: Arc<dyn PasswordHasher + Send + Sync>,
    /// Anti-bruteforce du login (en mémoire par défaut ; partagé en
    /// PostgreSQL via [`with_throttle`](Self::with_throttle)).
    pub throttle: Arc<dyn LoginThrottleStore>,
    /// Permissions accordées à l'inscription (rôle `editor` par défaut).
    pub signup_permissions: HashSet<String>,
    /// Hash leurre vérifié quand l'email est inconnu (égalisation du temps).
    dummy_hash: Arc<str>,
}

impl AccountsState {
    /// Construit l'état ; les comptes créés reçoivent le rôle `editor`.
    /// Échoue seulement si le hash leurre ne peut être calculé.
    pub fn new(
        auth: AuthState,
        users: Arc<dyn UserStore + Send + Sync>,
        hasher: Arc<dyn PasswordHasher + Send + Sync>,
    ) -> autome_core::Result<Self> {
        let dummy_hash = hasher.hash(DUMMY_PASSWORD)?;
        Ok(Self {
            auth,
            users,
            hasher,
            throttle: Arc::new(InMemoryLoginThrottle::new()),
            signup_permissions: Role::editor().permissions,
            dummy_hash: dummy_hash.into(),
        })
    }

    /// Remplace le compteur anti-bruteforce (ex. PostgreSQL, partagé entre
    /// répliques).
    pub fn with_throttle(mut self, throttle: Arc<dyn LoginThrottleStore>) -> Self {
        self.throttle = throttle;
        self
    }
}

// Permet l'extracteur [`AuthenticatedUser`] sur `/auth/logout-all`.
impl FromRef<AccountsState> for AuthState {
    fn from_ref(input: &AccountsState) -> Self {
        input.auth.clone()
    }
}

/// Body de `/auth/signup` et `/auth/login`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CredentialsBody {
    /// Adresse email (normalisée côté serveur : trim + minuscules).
    pub email: String,
    /// Mot de passe en clair (8 à 1024 caractères).
    pub password: String,
}

/// Body de `/auth/refresh` et `/auth/logout`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshBody {
    /// Refresh token obtenu au login/signup ou au refresh précédent.
    pub refresh_token: String,
}

/// Paire de jetons émise par signup/login/refresh.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TokenResponse {
    /// JWT d'accès, à présenter en `Authorization: Bearer`.
    pub access_token: String,
    /// JWT de rafraîchissement (usage unique).
    pub refresh_token: String,
    /// Toujours `Bearer`.
    pub token_type: String,
    /// Durée de vie de l'access token, en secondes.
    pub expires_in: i64,
}

/// Erreurs des handlers de comptes.
#[derive(Debug)]
pub enum AccountError {
    /// Saisie invalide (email, politique de mot de passe).
    BadRequest(String),
    /// Identifiants ou jeton refusés — message volontairement générique.
    Unauthorized,
    /// Email déjà associé à un compte.
    Conflict,
    /// Trop d'échecs de connexion : réessayer après `n` secondes.
    TooManyAttempts(i64),
    /// Erreur interne (store, hash).
    Internal(String),
}

impl IntoResponse for AccountError {
    fn into_response(self) -> Response {
        match self {
            AccountError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            AccountError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "invalid credentials").into_response()
            }
            AccountError::Conflict => {
                (StatusCode::CONFLICT, "email already registered").into_response()
            }
            AccountError::TooManyAttempts(secs) => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                "too many failed login attempts",
            )
                .into_response(),
            AccountError::Internal(msg) => {
                tracing::error!(error = %msg, "erreur interne des comptes");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

fn internal(e: impl std::fmt::Display) -> AccountError {
    AccountError::Internal(e.to_string())
}

/// Exécute un calcul Argon2 hors du runtime async.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> autome_core::Result<T> + Send + 'static,
) -> Result<T, AccountError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(internal)?
        .map_err(internal)
}

async fn audit(state: &AccountsState, actor: &str, action: &str, outcome: AuditOutcome) {
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let event = AuditEvent::new(now_ms, actor, action, outcome);
    if let Err(e) = state.auth.audit.record(event).await {
        tracing::error!(action, error = %e, "événement d'audit non enregistré");
    }
}

fn token_response(access_token: String, refresh_token: String) -> TokenResponse {
    TokenResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".into(),
        expires_in: ACCESS_TTL_SECONDS,
    }
}

/// Émet une paire à la version de session courante de l'utilisateur (#186).
async fn issue_pair(state: &AccountsState, user_id: &str) -> Result<TokenResponse, AccountError> {
    let sv = state
        .auth
        .revocation
        .session_version(user_id)
        .await
        .map_err(internal)?;
    let now = state.auth.clock.now_unix_seconds();
    let (access, refresh) = state
        .auth
        .token_service
        .issue_pair(user_id, sv, ACCESS_TTL_SECONDS, REFRESH_TTL_SECONDS, now)
        .map_err(internal)?;
    Ok(token_response(access, refresh))
}

/// `POST /auth/signup` — crée un compte (rôle `editor`) et le connecte.
#[utoipa::path(
    post,
    path = "/auth/signup",
    tag = "auth",
    request_body = CredentialsBody,
    responses(
        (status = 201, description = "Compte créé", body = TokenResponse),
        (status = 400, description = "Email ou mot de passe invalide"),
        (status = 409, description = "Email déjà enregistré")
    )
)]
pub async fn signup(
    State(state): State<AccountsState>,
    Json(body): Json<CredentialsBody>,
) -> Result<(StatusCode, Json<TokenResponse>), AccountError> {
    let email = normalize_email(&body.email);
    validate_email(&email).map_err(|e| AccountError::BadRequest(e.to_string()))?;
    validate_password(&body.password).map_err(|e| AccountError::BadRequest(e.to_string()))?;

    let hasher = Arc::clone(&state.hasher);
    let password = body.password;
    let hash = blocking(move || hasher.hash(&password)).await?;

    let user = match state
        .users
        .create(&email, &hash, &state.signup_permissions)
        .await
        .map_err(internal)?
    {
        CreateUser::Created(user) => user,
        CreateUser::EmailTaken => {
            audit(&state, &email, "auth.signup", AuditOutcome::Denied).await;
            return Err(AccountError::Conflict);
        }
    };
    audit(&state, &user.id, "auth.signup", AuditOutcome::Success).await;
    Ok((
        StatusCode::CREATED,
        Json(issue_pair(&state, &user.id).await?),
    ))
}

/// `POST /auth/login` — échange email + mot de passe contre une paire de jetons.
#[utoipa::path(
    post,
    path = "/auth/login",
    tag = "auth",
    request_body = CredentialsBody,
    responses(
        (status = 200, description = "Connecté", body = TokenResponse),
        (status = 401, description = "Identifiants invalides"),
        (status = 429, description = "Trop d'échecs — voir `Retry-After`")
    )
)]
pub async fn login(
    State(state): State<AccountsState>,
    Json(body): Json<CredentialsBody>,
) -> Result<Json<TokenResponse>, AccountError> {
    let email = normalize_email(&body.email);
    let now = state.auth.clock.now_unix_seconds();
    // Fail closed : sans compteur, pas de vérification de mot de passe.
    if let LoginAttempt::Blocked { retry_after_secs } = state
        .throttle
        .begin_attempt(&email, now)
        .await
        .map_err(internal)?
    {
        audit(&state, &email, "auth.login", AuditOutcome::Denied).await;
        return Err(AccountError::TooManyAttempts(retry_after_secs));
    }

    let user = state.users.find_by_email(&email).await.map_err(internal)?;
    // Email inconnu : on vérifie quand même contre le hash leurre pour que la
    // durée de réponse ne révèle pas l'existence du compte.
    let phc = match &user {
        Some(u) => u.password_hash.clone(),
        None => state.dummy_hash.to_string(),
    };
    let hasher = Arc::clone(&state.hasher);
    let password = body.password;
    // Un hash PHC mal formé en base (compte non migré) vaut un refus, pas une 500.
    let matches = blocking(move || Ok(hasher.verify(&password, &phc).unwrap_or(false))).await?;

    match user {
        Some(user) if matches => {
            // La connexion a réussi : un compteur non remis à zéro ne doit pas
            // la faire échouer (il expirera avec sa fenêtre).
            if let Err(e) = state.throttle.reset(&email).await {
                tracing::warn!(error = %e, "remise à zéro du compteur de connexion impossible");
            }
            audit(&state, &user.id, "auth.login", AuditOutcome::Success).await;
            Ok(Json(issue_pair(&state, &user.id).await?))
        }
        _ => {
            audit(&state, &email, "auth.login", AuditOutcome::Failure).await;
            Err(AccountError::Unauthorized)
        }
    }
}

/// `POST /auth/refresh` — rotation : le refresh présenté est révoqué et une
/// nouvelle paire est émise. Un refresh déjà utilisé est refusé (rejeu).
#[utoipa::path(
    post,
    path = "/auth/refresh",
    tag = "auth",
    request_body = RefreshBody,
    responses(
        (status = 200, description = "Nouvelle paire", body = TokenResponse),
        (status = 401, description = "Refresh invalide, expiré ou déjà utilisé")
    )
)]
pub async fn refresh(
    State(state): State<AccountsState>,
    Json(body): Json<RefreshBody>,
) -> Result<Json<TokenResponse>, AccountError> {
    let now = state.auth.clock.now_unix_seconds();
    let (access_token, refresh_token) = state
        .auth
        .token_service
        .rotate(
            &body.refresh_token,
            state.auth.revocation.as_ref(),
            ACCESS_TTL_SECONDS,
            REFRESH_TTL_SECONDS,
            now,
        )
        .await
        .map_err(|_| AccountError::Unauthorized)?;
    Ok(Json(token_response(access_token, refresh_token)))
}

/// `POST /auth/logout` — ferme la session côté serveur.
///
/// Révoque le refresh présenté (plus de rotation possible) et, s'il est fourni
/// en `Authorization: Bearer`, l'access token **du même utilisateur** (refusé
/// partout jusqu'à son expiration). Idempotent : un refresh déjà révoqué
/// répond aussi 204. Un Bearer absent, invalide ou d'un autre utilisateur
/// n'empêche pas la déconnexion : il est simplement ignoré.
#[utoipa::path(
    post,
    path = "/auth/logout",
    tag = "auth",
    request_body = RefreshBody,
    params(
        ("Authorization" = Option<String>, Header,
         description = "`Bearer <access_token>` de la session, révoqué aussi s'il est fourni")
    ),
    responses(
        (status = 204, description = "Session fermée (ou déjà fermée)"),
        (status = 401, description = "Refresh invalide ou expiré")
    )
)]
pub async fn logout(
    State(state): State<AccountsState>,
    headers: HeaderMap,
    Json(body): Json<RefreshBody>,
) -> Result<StatusCode, AccountError> {
    let now = state.auth.clock.now_unix_seconds();
    let svc = &state.auth.token_service;
    let refresh = match svc.verify(&body.refresh_token, now) {
        Ok(claims) if claims.kind == TokenKind::Refresh => claims,
        _ => return Err(AccountError::Unauthorized),
    };
    let revocation = &state.auth.revocation;
    revocation
        .revoke(&refresh.jti, refresh.exp.saturating_mul(1_000))
        .await
        .map_err(internal)?;

    if let Ok(token) = bearer_token(&headers) {
        match svc.verify(token, now) {
            Ok(access) if access.kind == TokenKind::Access && access.sub == refresh.sub => {
                revocation
                    .revoke(&access.jti, access.exp.saturating_mul(1_000))
                    .await
                    .map_err(internal)?;
            }
            _ => tracing::debug!("logout : Bearer ignoré (invalide ou autre utilisateur)"),
        }
    }

    audit(&state, &refresh.sub, "auth.logout", AuditOutcome::Success).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/logout-all` — ferme toutes les sessions de l'utilisateur
/// authentifié, sur tous les appareils, y compris celle de la requête.
///
/// Incrémente sa version de session : tous ses access et refresh tokens émis
/// jusqu'ici sont refusés (401) ; une nouvelle connexion ouvre une session
/// valide. Réponse 204.
#[utoipa::path(
    post,
    path = "/auth/logout-all",
    tag = "auth",
    responses(
        (status = 204, description = "Toutes les sessions sont fermées"),
        (status = 401, description = "Access token absent, invalide ou déjà révoqué")
    ),
    security(("bearer" = []))
)]
pub async fn logout_all(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
) -> Result<StatusCode, AccountError> {
    let version = state
        .auth
        .revocation
        .bump_session_version(user.user_id())
        .await
        .map_err(internal)?;
    tracing::info!(
        user_id = user.user_id(),
        session_version = version,
        "toutes les sessions fermées"
    );
    audit(
        &state,
        user.user_id(),
        "auth.logout_all",
        AuditOutcome::Success,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Sous-router `/signup`, `/login`, `/refresh`, `/logout` (publics) et
/// `/logout-all` (Bearer) — à imbriquer sous
/// `/auth` avec [`crate::auth::protected_router`].
pub fn accounts_router(state: AccountsState) -> Router {
    Router::new()
        .route("/signup", post(signup))
        .route("/login", post(login))
        .route("/refresh", post(refresh))
        .route("/logout", post(logout))
        .route("/logout-all", post(logout_all))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{protected_router, Clock};
    use autome_core::{
        Argon2PasswordHasher, AuditSink, InMemoryAuditSink, InMemoryPermissionStore,
        InMemoryRevocationStore, InMemoryUserStore, PermissionStore, TokenKind, TokenService,
    };
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const START: i64 = 1_700_000_000;

    /// Horloge ajustable (fenêtre anti-bruteforce, expirations).
    struct MovableClock(Mutex<i64>);
    impl MovableClock {
        fn advance(&self, secs: i64) {
            *self.0.lock().unwrap() += secs;
        }
    }
    impl Clock for MovableClock {
        fn now_unix_seconds(&self) -> i64 {
            *self.0.lock().unwrap()
        }
    }

    /// Hasher factice (rapide) : `plain:<mdp>`. Argon2 réel testé à part.
    struct PlainHasher;
    impl PasswordHasher for PlainHasher {
        fn hash(&self, plain: &str) -> autome_core::Result<String> {
            Ok(format!("plain:{plain}"))
        }
        fn verify(&self, plain: &str, phc: &str) -> autome_core::Result<bool> {
            match phc.strip_prefix("plain:") {
                Some(stored) => Ok(stored == plain),
                None => Err(autome_core::Error::Invalid("hash mal formé".into())),
            }
        }
    }

    struct Harness {
        app: Router,
        clock: Arc<MovableClock>,
        perms: Arc<InMemoryPermissionStore>,
        users: Arc<InMemoryUserStore>,
        audit: Arc<InMemoryAuditSink>,
    }

    fn harness_with(hasher: Arc<dyn PasswordHasher + Send + Sync>) -> Harness {
        let clock = Arc::new(MovableClock(Mutex::new(START)));
        let perms = Arc::new(InMemoryPermissionStore::new());
        let users = Arc::new(InMemoryUserStore::new(perms.clone()));
        let audit = Arc::new(InMemoryAuditSink::new());
        let auth = AuthState::new(
            TokenService::new(SECRET),
            Arc::new(InMemoryRevocationStore::new()),
            perms.clone(),
            audit.clone(),
        )
        .with_clock(clock.clone());
        let accounts = AccountsState::new(auth.clone(), users.clone(), hasher).unwrap();
        let app = protected_router(auth).merge(accounts_router(accounts));
        Harness {
            app,
            clock,
            perms,
            users,
            audit,
        }
    }

    fn harness() -> Harness {
        harness_with(Arc::new(PlainHasher))
    }

    async fn post(app: &Router, path: &str, body: serde_json::Value) -> Response {
        app.clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post_bearer(
        app: &Router,
        path: &str,
        bearer: &str,
        body: serde_json::Value,
    ) -> Response {
        app.clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .header("Authorization", format!("Bearer {bearer}"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn whoami_status(app: &Router, access: &str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::get("/whoami")
                    .header("Authorization", format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    async fn tokens(resp: Response) -> TokenResponse {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn text(resp: Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn creds(email: &str, password: &str) -> serde_json::Value {
        json!({ "email": email, "password": password })
    }

    fn verify(token: &str) -> autome_core::Claims {
        TokenService::new(SECRET).verify(token, START + 1).unwrap()
    }

    #[tokio::test]
    async fn signup_creates_editor_account_and_returns_token_pair() {
        let h = harness();
        let resp = post(
            &h.app,
            "/signup",
            creds(" Alice@Example.io ", "s3cret-pass"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let pair = tokens(resp).await;
        assert_eq!(pair.token_type, "Bearer");
        assert_eq!(pair.expires_in, ACCESS_TTL_SECONDS);

        let access = verify(&pair.access_token);
        assert_eq!(access.kind, TokenKind::Access);
        assert_eq!(access.exp, START + ACCESS_TTL_SECONDS);
        let refresh = verify(&pair.refresh_token);
        assert_eq!(refresh.kind, TokenKind::Refresh);
        assert_eq!(refresh.sub, access.sub);

        // Email normalisé, mot de passe jamais stocké en clair.
        let user = h
            .users
            .find_by_email("alice@example.io")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.id, access.sub);
        assert_eq!(user.password_hash, "plain:s3cret-pass");
        assert_eq!(
            h.perms.permissions_for(&user.id).await.unwrap(),
            Role::editor().permissions
        );
    }

    #[tokio::test]
    async fn signup_rejects_duplicate_email_case_insensitively() {
        let h = harness();
        post(&h.app, "/signup", creds("bob@example.io", "password-1")).await;
        let resp = post(&h.app, "/signup", creds("BOB@example.io", "password-2")).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        // Le premier mot de passe reste le bon.
        let ok = post(&h.app, "/login", creds("bob@example.io", "password-1")).await;
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn signup_rejects_invalid_input() {
        let h = harness();
        let bad_email = post(&h.app, "/signup", creds("not-an-email", "long-enough")).await;
        assert_eq!(bad_email.status(), StatusCode::BAD_REQUEST);
        let short = post(&h.app, "/signup", creds("c@example.io", "short")).await;
        assert_eq!(short.status(), StatusCode::BAD_REQUEST);
        assert!(text(short).await.contains("trop court"));
        let missing_field = post(&h.app, "/signup", json!({ "email": "c@example.io" })).await;
        assert!(missing_field.status().is_client_error());
    }

    #[tokio::test]
    async fn login_succeeds_with_right_password() {
        let h = harness();
        post(&h.app, "/signup", creds("dan@example.io", "correct-horse")).await;
        let resp = post(&h.app, "/login", creds("DAN@example.io", "correct-horse")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let pair = tokens(resp).await;
        let user = h
            .users
            .find_by_email("dan@example.io")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(verify(&pair.access_token).sub, user.id);
    }

    #[tokio::test]
    async fn login_failures_are_indistinguishable() {
        let h = harness();
        post(&h.app, "/signup", creds("eve@example.io", "correct-horse")).await;
        let wrong = post(&h.app, "/login", creds("eve@example.io", "wrong-horse")).await;
        let unknown = post(&h.app, "/login", creds("nobody@example.io", "whatever-1")).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(text(wrong).await, text(unknown).await);
    }

    #[tokio::test]
    async fn login_with_malformed_stored_hash_is_401_not_500() {
        let h = harness();
        h.users
            .create("legacy@example.io", "argon2-placeholder", &HashSet::new())
            .await
            .unwrap();
        let resp = post(&h.app, "/login", creds("legacy@example.io", "anything-1")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_is_throttled_after_repeated_failures() {
        let h = harness();
        post(&h.app, "/signup", creds("fay@example.io", "correct-horse")).await;
        for _ in 0..MAX_LOGIN_FAILURES {
            let r = post(&h.app, "/login", creds("fay@example.io", "wrong-horse")).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        // Bloqué, même avec le bon mot de passe.
        let blocked = post(&h.app, "/login", creds("fay@example.io", "correct-horse")).await;
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            blocked.headers()[header::RETRY_AFTER],
            LOGIN_WINDOW_SECONDS.to_string()
        );
        // Un autre compte n'est pas affecté.
        let other = post(&h.app, "/login", creds("other@example.io", "whatever-1")).await;
        assert_eq!(other.status(), StatusCode::UNAUTHORIZED);

        h.clock.advance(LOGIN_WINDOW_SECONDS);
        let ok = post(&h.app, "/login", creds("fay@example.io", "correct-horse")).await;
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn successful_login_resets_the_counter() {
        let h = harness();
        post(&h.app, "/signup", creds("gus@example.io", "correct-horse")).await;
        for _ in 0..MAX_LOGIN_FAILURES - 1 {
            post(&h.app, "/login", creds("gus@example.io", "wrong-horse")).await;
        }
        let ok = post(&h.app, "/login", creds("gus@example.io", "correct-horse")).await;
        assert_eq!(ok.status(), StatusCode::OK);
        let wrong = post(&h.app, "/login", creds("gus@example.io", "wrong-horse")).await;
        assert_eq!(
            wrong.status(),
            StatusCode::UNAUTHORIZED,
            "compteur remis à zéro"
        );
    }

    #[tokio::test]
    async fn refresh_rotates_and_rejects_replay() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("hal@example.io", "correct-horse")).await).await;

        let resp = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let next = tokens(resp).await;
        assert_ne!(next.refresh_token, pair.refresh_token);
        assert_eq!(
            verify(&next.access_token).sub,
            verify(&pair.access_token).sub
        );

        let replay = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
        let with_access = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": next.access_token }),
        )
        .await;
        assert_eq!(with_access.status(), StatusCode::UNAUTHORIZED);
        let garbage = post(&h.app, "/refresh", json!({ "refresh_token": "x.y.z" })).await;
        assert_eq!(garbage.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_revokes_refresh_and_access_of_the_session() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("kim@example.io", "correct-horse")).await).await;
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );

        let resp = post_bearer(
            &h.app,
            "/logout",
            &pair.access_token,
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::UNAUTHORIZED
        );
        let refresh = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(refresh.status(), StatusCode::UNAUTHORIZED);

        // Une nouvelle connexion ouvre une nouvelle session.
        let again =
            tokens(post(&h.app, "/login", creds("kim@example.io", "correct-horse")).await).await;
        assert_eq!(
            whoami_status(&h.app, &again.access_token).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn logout_is_idempotent() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("lea@example.io", "correct-horse")).await).await;
        let body = json!({ "refresh_token": pair.refresh_token });
        assert_eq!(
            post(&h.app, "/logout", body.clone()).await.status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            post(&h.app, "/logout", body).await.status(),
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn logout_without_bearer_revokes_only_the_refresh() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("max@example.io", "correct-horse")).await).await;
        let resp = post(
            &h.app,
            "/logout",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let refresh = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(refresh.status(), StatusCode::UNAUTHORIZED);
        // L'access non présenté reste valide jusqu'à son expiration (15 min).
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn logout_ignores_a_bearer_of_another_user() {
        let h = harness();
        let alice =
            tokens(post(&h.app, "/signup", creds("ann@example.io", "correct-horse")).await).await;
        let bob =
            tokens(post(&h.app, "/signup", creds("bob@example.io", "correct-horse")).await).await;
        let resp = post_bearer(
            &h.app,
            "/logout",
            &bob.access_token,
            json!({ "refresh_token": alice.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            whoami_status(&h.app, &bob.access_token).await,
            StatusCode::OK
        );
        // Un Bearer illisible n'empêche pas non plus la déconnexion.
        let resp = post_bearer(
            &h.app,
            "/logout",
            "x.y.z",
            json!({ "refresh_token": bob.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn logout_rejects_what_is_not_a_valid_refresh() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("ned@example.io", "correct-horse")).await).await;
        for bad in [pair.access_token.as_str(), "x.y.z", ""] {
            let resp = post(&h.app, "/logout", json!({ "refresh_token": bad })).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{bad:?}");
        }
        // Refresh expiré : plus rien à révoquer, refusé comme invalide.
        h.clock.advance(REFRESH_TTL_SECONDS + 1);
        let resp = post(
            &h.app,
            "/logout",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let events = h.audit.list().await.unwrap();
        assert!(!events.iter().any(|e| e.action == "auth.logout"));
    }

    #[tokio::test]
    async fn logout_is_audited_under_the_user() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("oli@example.io", "correct-horse")).await).await;
        post(
            &h.app,
            "/logout",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        let user = verify(&pair.access_token).sub;
        let events = h.audit.list().await.unwrap();
        assert!(events.iter().any(|e| e.action == "auth.logout"
            && e.actor == user
            && e.outcome == AuditOutcome::Success));
    }

    async fn refresh_status(app: &Router, refresh_token: &str) -> StatusCode {
        post(app, "/refresh", json!({ "refresh_token": refresh_token }))
            .await
            .status()
    }

    #[tokio::test]
    async fn logout_all_closes_every_session_of_the_user() {
        let h = harness();
        let laptop =
            tokens(post(&h.app, "/signup", creds("pia@example.io", "correct-horse")).await).await;
        let phone =
            tokens(post(&h.app, "/login", creds("pia@example.io", "correct-horse")).await).await;
        // Session rafraîchie avant la déconnexion générale : concernée aussi.
        let rotated = tokens(
            post(
                &h.app,
                "/refresh",
                json!({ "refresh_token": phone.refresh_token }),
            )
            .await,
        )
        .await;
        let bob =
            tokens(post(&h.app, "/signup", creds("bob@example.io", "correct-horse")).await).await;

        let resp = post_bearer(&h.app, "/logout-all", &laptop.access_token, json!({})).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        for pair in [&laptop, &rotated] {
            assert_eq!(
                whoami_status(&h.app, &pair.access_token).await,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                refresh_status(&h.app, &pair.refresh_token).await,
                StatusCode::UNAUTHORIZED
            );
        }
        // Les autres utilisateurs ne sont pas touchés.
        assert_eq!(
            whoami_status(&h.app, &bob.access_token).await,
            StatusCode::OK
        );
        assert_eq!(
            refresh_status(&h.app, &bob.refresh_token).await,
            StatusCode::OK
        );

        // Une nouvelle connexion ouvre une session valide, à la nouvelle version.
        let again =
            tokens(post(&h.app, "/login", creds("pia@example.io", "correct-horse")).await).await;
        assert_eq!(verify(&again.access_token).sv, 1);
        assert_eq!(
            whoami_status(&h.app, &again.access_token).await,
            StatusCode::OK
        );
        let next = tokens(
            post(
                &h.app,
                "/refresh",
                json!({ "refresh_token": again.refresh_token }),
            )
            .await,
        )
        .await;
        assert_eq!(verify(&next.refresh_token).sv, 1);
        assert_eq!(
            whoami_status(&h.app, &next.access_token).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn logout_all_requires_an_access_token() {
        let h = harness();
        let pair = tokens(
            post(
                &h.app,
                "/signup",
                creds("quinn@example.io", "correct-horse"),
            )
            .await,
        )
        .await;
        let missing = post(&h.app, "/logout-all", json!({})).await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        let with_refresh = post_bearer(&h.app, "/logout-all", &pair.refresh_token, json!({})).await;
        assert_eq!(with_refresh.status(), StatusCode::UNAUTHORIZED);
        // Rien n'a été fermé.
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );
        // Un second logout-all avec un jeton déjà fermé est refusé.
        post_bearer(&h.app, "/logout-all", &pair.access_token, json!({})).await;
        let again = post_bearer(&h.app, "/logout-all", &pair.access_token, json!({})).await;
        assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_all_is_audited_under_the_user() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("rae@example.io", "correct-horse")).await).await;
        post_bearer(&h.app, "/logout-all", &pair.access_token, json!({})).await;
        let user = verify(&pair.access_token).sub;
        let events = h.audit.list().await.unwrap();
        assert!(events.iter().any(|e| e.action == "auth.logout_all"
            && e.actor == user
            && e.outcome == AuditOutcome::Success));
    }

    #[tokio::test]
    async fn signup_and_login_are_audited() {
        let h = harness();
        post(&h.app, "/signup", creds("ivy@example.io", "correct-horse")).await;
        post(&h.app, "/login", creds("ivy@example.io", "wrong-horse")).await;
        let events = h.audit.list().await.unwrap();
        let actions: Vec<_> = events
            .iter()
            .map(|e| (e.action.as_str(), e.outcome))
            .collect();
        assert!(actions.contains(&("auth.signup", AuditOutcome::Success)));
        assert!(actions.contains(&("auth.login", AuditOutcome::Failure)));
    }

    #[tokio::test]
    async fn real_argon2_end_to_end_signup_login_whoami() {
        let h = harness_with(Arc::new(Argon2PasswordHasher::new()));
        let signup = post(&h.app, "/signup", creds("joe@example.io", "correct-horse")).await;
        assert_eq!(signup.status(), StatusCode::CREATED);
        let stored = h
            .users
            .find_by_email("joe@example.io")
            .await
            .unwrap()
            .unwrap();
        assert!(stored.password_hash.starts_with("$argon2id$"));

        let pair =
            tokens(post(&h.app, "/login", creds("joe@example.io", "correct-horse")).await).await;
        let whoami = h
            .app
            .clone()
            .oneshot(
                Request::get("/whoami")
                    .header("Authorization", format!("Bearer {}", pair.access_token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(whoami.status(), StatusCode::OK);
        assert_eq!(text(whoami).await, format!("hello {}", stored.id));

        let wrong = post(&h.app, "/login", creds("joe@example.io", "wrong-horse")).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
}

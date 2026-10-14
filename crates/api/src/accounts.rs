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
//! - **Anti-bruteforce** ([`LoginThrottle`]) : au-delà de
//!   [`MAX_LOGIN_FAILURES`] tentatives sans succès sur un email dans la
//!   fenêtre, 429 jusqu'à sa fin. Compteur **in-process** : par réplique, perdu au redémarrage.
//! - Le refresh réutilise [`autome_core::TokenService::rotate`] : le refresh
//!   présenté est révoqué, un rejeu est refusé.
//! - Chaque inscription/connexion produit un event d'audit `auth.signup` /
//!   `auth.login`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use autome_core::{
    normalize_email, validate_email, validate_password, AuditEvent, AuditOutcome, CreateUser,
    PasswordHasher, Role, UserStore,
};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::AuthState;

/// Durée de vie d'un access token (15 min).
pub const ACCESS_TTL_SECONDS: i64 = 15 * 60;
/// Durée de vie d'un refresh token (7 jours).
pub const REFRESH_TTL_SECONDS: i64 = 7 * 24 * 3600;
/// Tentatives de connexion sans succès tolérées par email et par fenêtre.
pub const MAX_LOGIN_FAILURES: u32 = 5;
/// Fenêtre du compteur d'échecs (15 min).
pub const LOGIN_WINDOW_SECONDS: i64 = 15 * 60;
/// Au-delà, les fenêtres expirées sont purgées (borne mémoire face à un
/// attaquant qui varie les emails).
const THROTTLE_PRUNE_THRESHOLD: usize = 10_000;
/// Mot de passe du hash leurre (jamais attribué à un compte).
const DUMMY_PASSWORD: &str = "autome-dummy-password-for-timing";

/// Compteur de tentatives de connexion par email (fenêtre fixe).
///
/// Une tentative est **comptée avant** la vérification du mot de passe, sous
/// le même verrou que le contrôle de blocage : une rafale de requêtes
/// parallèles ne peut pas dépasser le quota en passant le contrôle avant que
/// les premiers échecs soient enregistrés. Une connexion réussie remet le
/// compteur à zéro.
#[derive(Debug, Default)]
pub struct LoginThrottle {
    /// email → (tentatives, début de fenêtre en secondes Unix).
    attempts: Mutex<HashMap<String, (u32, i64)>>,
}

impl LoginThrottle {
    /// Construit un compteur vide.
    pub fn new() -> Self {
        Self::default()
    }

    /// Réserve une tentative pour `email`. `Err(secs)` si le quota de la
    /// fenêtre est épuisé : réessayer dans `secs` secondes.
    pub fn begin_attempt(&self, email: &str, now: i64) -> Result<(), i64> {
        let Ok(mut attempts) = self.attempts.lock() else {
            // Verrou empoisonné : on n'empêche pas la connexion (le hash
            // Argon2 reste le coût plancher d'une tentative).
            return Ok(());
        };
        if attempts.len() >= THROTTLE_PRUNE_THRESHOLD {
            attempts.retain(|_, (_, start)| now - *start < LOGIN_WINDOW_SECONDS);
        }
        let entry = attempts.entry(email.to_string()).or_insert((0, now));
        if now - entry.1 >= LOGIN_WINDOW_SECONDS {
            *entry = (0, now);
        }
        if entry.0 >= MAX_LOGIN_FAILURES {
            return Err(entry.1 + LOGIN_WINDOW_SECONDS - now);
        }
        entry.0 += 1;
        Ok(())
    }

    /// Remet le compteur de `email` à zéro (connexion réussie).
    pub fn reset(&self, email: &str) {
        if let Ok(mut attempts) = self.attempts.lock() {
            attempts.remove(email);
        }
    }
}

/// État des handlers de comptes.
#[derive(Clone)]
pub struct AccountsState {
    /// État d'authentification (émission/rotation des jetons, audit, horloge).
    pub auth: AuthState,
    /// Persistance des comptes.
    pub users: Arc<dyn UserStore + Send + Sync>,
    /// Hash/vérification des mots de passe.
    pub hasher: Arc<dyn PasswordHasher + Send + Sync>,
    /// Anti-bruteforce du login.
    pub throttle: Arc<LoginThrottle>,
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
            throttle: Arc::new(LoginThrottle::new()),
            signup_permissions: Role::editor().permissions,
            dummy_hash: dummy_hash.into(),
        })
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

/// Body de `/auth/refresh`.
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

fn issue_pair(state: &AccountsState, user_id: &str) -> Result<TokenResponse, AccountError> {
    let now = state.auth.clock.now_unix_seconds();
    let svc = &state.auth.token_service;
    Ok(TokenResponse {
        access_token: svc
            .issue_access(user_id, ACCESS_TTL_SECONDS, now)
            .map_err(internal)?,
        refresh_token: svc
            .issue_refresh(user_id, REFRESH_TTL_SECONDS, now)
            .map_err(internal)?,
        token_type: "Bearer".into(),
        expires_in: ACCESS_TTL_SECONDS,
    })
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
    Ok((StatusCode::CREATED, Json(issue_pair(&state, &user.id)?)))
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
    if let Err(secs) = state.throttle.begin_attempt(&email, now) {
        audit(&state, &email, "auth.login", AuditOutcome::Denied).await;
        return Err(AccountError::TooManyAttempts(secs));
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
            state.throttle.reset(&email);
            audit(&state, &user.id, "auth.login", AuditOutcome::Success).await;
            Ok(Json(issue_pair(&state, &user.id)?))
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
    Ok(Json(TokenResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".into(),
        expires_in: ACCESS_TTL_SECONDS,
    }))
}

/// Sous-router public `/signup`, `/login`, `/refresh` — à imbriquer sous
/// `/auth` avec [`crate::auth::protected_router`].
pub fn accounts_router(state: AccountsState) -> Router {
    Router::new()
        .route("/signup", post(signup))
        .route("/login", post(login))
        .route("/refresh", post(refresh))
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

    #[test]
    fn throttle_window_resets_after_expiry() {
        let t = LoginThrottle::new();
        for _ in 0..MAX_LOGIN_FAILURES {
            assert!(t.begin_attempt("a", START).is_ok());
        }
        assert_eq!(
            t.begin_attempt("a", START + 60),
            Err(LOGIN_WINDOW_SECONDS - 60)
        );
        assert!(t.begin_attempt("a", START + LOGIN_WINDOW_SECONDS).is_ok());
    }
}

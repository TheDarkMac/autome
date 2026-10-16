//! Primitives d'authentification (F6.1).
//!
//! Trois piliers indépendants, branchés par l'API (F7) :
//!
//! | Module           | Rôle                                                         |
//! |------------------|--------------------------------------------------------------|
//! | [`password`]     | Hash/verify de mot de passe (Argon2id via PHC, F6.1.a)       |
//! | [`jwt`]          | Émission/vérification JWT HS256 + rotation (F6.1.b/F6.1.d)   |
//! | [`revocation`]   | Liste de révocation par `jti` avec GC opportuniste (F6.1.d)  |
//! | [`users`]        | Comptes (email + hash) et règles de saisie (backlog auth)    |
//! | [`throttle`]     | Anti-bruteforce de la connexion, partageable (#182)          |
//!
//! Le **middleware HTTP** (extractor axum) qui consomme ces primitives
//! vit dans le crate `autome-api` (F6.1.c) — `core` reste agnostique
//! du framework HTTP.

pub mod jwt;
pub mod password;
pub mod revocation;
pub mod throttle;
pub mod users;

pub use jwt::{Claims, TokenKind, TokenService};
pub use password::{Argon2PasswordHasher, PasswordHasher};
pub use revocation::{InMemoryRevocationStore, RevocationStore, TokenState};
pub use throttle::{
    email_throttle_key, InMemoryLoginThrottle, LoginAttempt, LoginThrottleStore,
    LOGIN_WINDOW_SECONDS, MAX_LOGIN_FAILURES,
};
pub use users::{
    normalize_email, validate_email, validate_password, CreateUser, InMemoryUserStore, UserPage,
    UserRecord, UserStore, UserSummary,
};

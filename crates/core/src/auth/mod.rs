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
//! | [`erasure`]      | Effacement d'un compte et de ses données (RGPD, #229)        |
//! | [`common_passwords`] | Refus des mots de passe courants ou compromis (#245)     |
//! | [`mfa`]          | Persistance du second facteur TOTP (#245)                    |
//!
//! Le **middleware HTTP** (extractor axum) qui consomme ces primitives
//! vit dans le crate `autome-api` (F6.1.c) — `core` reste agnostique
//! du framework HTTP.

pub mod common_passwords;
pub mod erasure;
pub mod jwt;
pub mod mfa;
pub mod password;
pub mod revocation;
pub mod throttle;
pub mod users;

pub use common_passwords::{
    BreachedPasswordSource, CommonPasswords, PasswordWeakness, COMPROMISED_PASSWORD_MESSAGE,
};
pub use erasure::{erasure_pseudonym, AccountEraser, ErasureReport, ERASED_PREFIX};
pub use jwt::{Claims, TokenKind, TokenService};
pub use mfa::{InMemoryMfaStore, MfaRecord, MfaStore};
pub use password::{Argon2PasswordHasher, PasswordHasher};
pub use revocation::{InMemoryRevocationStore, RevocationStore, TokenState};
pub use throttle::{
    email_throttle_key, InMemoryLoginThrottle, LoginAttempt, LoginThrottleStore,
    LOGIN_WINDOW_SECONDS, MAX_LOGIN_FAILURES,
};
pub use users::{
    has_usable_password, normalize_email, unusable_password_hash, validate_email,
    validate_password, CreateUser, InMemoryUserStore, UserPage, UserRecord, UserStore, UserSummary,
    UNUSABLE_PASSWORD_PREFIX,
};

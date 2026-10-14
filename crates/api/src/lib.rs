//! `autome-api` — bibliothèque du serveur HTTP.
//!
//! Le binaire (`src/main.rs`) reste un thin wrapper ; la logique réutilisable
//! (middleware auth, routers de domaine) vit ici pour être testable et
//! consommable depuis des tests d'intégration.
//!
//! Modules :
//! - [`accounts`] : inscription, connexion et rotation des jetons
//!   (`/auth/signup|login|refresh`).
//! - [`auth`] (F6.1.c) : extractor axum `AuthenticatedUser` qui consomme
//!   les primitives `core::auth` (TokenService, RevocationStore).
//! - [`tls`] (F6.4) : chargement de la config TLS rustls depuis des
//!   fichiers PEM (cert chain + clé privée).

pub mod accounts;
pub mod auth;
pub mod credentials;
pub mod metrics;
pub mod observability;
pub mod openapi;
pub mod runs;
pub mod runtime;
pub mod tls;
pub mod workflows;

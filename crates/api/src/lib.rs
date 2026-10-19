//! `autome-api` — bibliothèque du serveur HTTP.
//!
//! Le binaire (`src/main.rs`) reste un thin wrapper ; la logique réutilisable
//! (middleware auth, routers de domaine) vit ici pour être testable et
//! consommable depuis des tests d'intégration.
//!
//! Modules :
//! - [`accounts`] : inscription, connexion et rotation des jetons
//!   (`/auth/signup|login|refresh`).
//! - [`admin`] (#208) : administration des utilisateurs (`/admin/users`).
//! - [`email_verification`] (#231) : vérification de l'adresse email.
//! - [`erasure`] (#229) : effacement d'un compte et de ses données (RGPD).
//! - [`mail`] et [`password_reset`] (#204) : envoi d'emails (SMTP) et jetons
//!   de réinitialisation du mot de passe ; [`signed_link`] : jetons sans état
//!   des liens envoyés par email.
//! - [`auth`] (F6.1.c) : extractor axum `AuthenticatedUser` qui consomme
//!   les primitives `core::auth` (TokenService, RevocationStore).
//! - [`tls`] (F6.4) : chargement de la config TLS rustls depuis des
//!   fichiers PEM (cert chain + clé privée).

pub mod accounts;
pub mod admin;
pub mod auth;
pub mod client_ip;
pub mod cors;
pub mod credentials;
pub mod email_verification;
pub mod erasure;
pub mod mail;
pub mod metrics;
pub mod observability;
pub mod openapi;
pub mod password_reset;
pub mod probes;
pub mod runs;
pub mod runtime;
pub mod shutdown;
pub mod signed_link;
pub mod tls;
pub mod webhooks;
pub mod workflows;

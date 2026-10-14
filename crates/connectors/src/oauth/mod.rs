//! OAuth2 — Authorization Code + PKCE pour les connecteurs tiers (F6.2).
//!
//! Pipeline complet :
//!
//! 1. [`provider::OAuthProviderConfig`] : déclaration d'un provider
//!    (client_id, urls, scopes, redirect_uri).
//!    [`provider::OAuthRegistry`] indexe N providers par `provider_id`.
//! 2. [`client::OAuthClient::start_authorization`] : génère PKCE
//!    (verifier + challenge S256), construit l'URL d'autorisation à
//!    présenter à l'utilisateur. La caller stocke `state` +
//!    `code_verifier` côté serveur, en attendant le callback.
//! 3. Callback HTTP (handled par l'API) : reçoit `code` + `state`,
//!    la caller cherche le `code_verifier` correspondant.
//! 4. [`client::OAuthClient::exchange_code`] : échange `code` contre
//!    une paire `(access_token, refresh_token)` via POST
//!    form-urlencoded.
//! 5. [`store::ConnectorTokenStore::store`] : conserve la
//!    [`client::TokenSet`] chiffrée via `EnvelopeCipher` (F6.3).
//! 6. Plus tard, [`client::OAuthClient::refresh`] échange le refresh
//!    token contre une nouvelle paire (la caller doit re-store) — piloté par
//!    [`refresh`] côté worker quand l'access token expire.

pub mod client;
pub mod pkce;
pub mod provider;
pub mod refresh;
pub mod store;

pub use client::{AuthorizationStart, OAuthClient, TokenSet};
pub use pkce::{generate_pkce, generate_state, PkcePair};
pub use provider::{OAuthProviderConfig, OAuthRegistry};
pub use refresh::{merge_refreshed, needs_refresh, TokenRefresher, REFRESH_SKEW_MS};
pub use store::{open_tokens, seal_tokens, ConnectorTokenStore, InMemoryConnectorTokenStore};

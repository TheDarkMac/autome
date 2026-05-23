//! Configuration et registre des providers OAuth2 (F6.2.c).
//!
//! [`OAuthProviderConfig`] décrit un provider (Google, Slack, Notion, GitHub,
//! …). [`OAuthRegistry`] en indexe plusieurs par `provider_id` — la caller
//! passe `provider_id` aux helpers du module pour dispatcher.

use std::collections::HashMap;

use autome_core::{Error, Result};

/// Configuration d'un provider OAuth2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProviderConfig {
    /// Identifiant stable (`"google"`, `"github"`, `"slack"`, …).
    pub provider_id: String,
    /// `client_id` enregistré chez le provider.
    pub client_id: String,
    /// `client_secret` (sera tiré d'un secret chiffré F6.3 en prod).
    pub client_secret: String,
    /// URL d'autorisation (ex. `https://accounts.google.com/o/oauth2/v2/auth`).
    pub authorize_url: String,
    /// URL d'échange du code et de refresh (ex. `https://oauth2.googleapis.com/token`).
    pub token_url: String,
    /// `redirect_uri` enregistré chez le provider.
    pub redirect_uri: String,
    /// Scopes demandés.
    pub scopes: Vec<String>,
}

impl OAuthProviderConfig {
    /// Builder minimal.
    pub fn new(
        provider_id: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        authorize_url: impl Into<String>,
        token_url: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            authorize_url: authorize_url.into(),
            token_url: token_url.into(),
            redirect_uri: redirect_uri.into(),
            scopes: Vec::new(),
        }
    }

    /// Builder : ajoute des scopes.
    pub fn with_scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }
}

/// Registre des providers OAuth — un `HashMap<provider_id, config>`.
#[derive(Debug, Default, Clone)]
pub struct OAuthRegistry {
    providers: HashMap<String, OAuthProviderConfig>,
}

impl OAuthRegistry {
    /// Crée un registre vide.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enregistre un provider. Écrase silencieusement une config existante
    /// du même `provider_id` (utile pour rotation client_secret).
    pub fn register(&mut self, config: OAuthProviderConfig) {
        self.providers.insert(config.provider_id.clone(), config);
    }

    /// Récupère la config d'un provider. `Err` si inconnu.
    pub fn get(&self, provider_id: &str) -> Result<&OAuthProviderConfig> {
        self.providers
            .get(provider_id)
            .ok_or_else(|| Error::Invalid(format!("oauth : provider '{provider_id}' inconnu")))
    }

    /// Liste les `provider_id` enregistrés (ordre non-déterministe).
    pub fn list(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    /// `true` si aucun provider enregistré.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn google() -> OAuthProviderConfig {
        OAuthProviderConfig::new(
            "google",
            "client-id-google",
            "secret-google",
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
            "https://app.autome/oauth/callback/google",
        )
        .with_scopes(["openid", "email", "profile"])
    }

    #[test]
    fn register_then_get_round_trips_config() {
        let mut r = OAuthRegistry::new();
        r.register(google());
        let cfg = r.get("google").unwrap();
        assert_eq!(cfg.client_id, "client-id-google");
        assert_eq!(cfg.scopes.len(), 3);
        assert!(cfg.scopes.contains(&"email".to_string()));
    }

    #[test]
    fn get_unknown_provider_errors() {
        let r = OAuthRegistry::new();
        let err = r.get("nope").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    #[test]
    fn register_overwrites_same_provider_id() {
        let mut r = OAuthRegistry::new();
        r.register(google());
        let updated = OAuthProviderConfig::new(
            "google",
            "new-client",
            "new-secret",
            "https://x/auth",
            "https://x/token",
            "https://app.autome/oauth/callback/google",
        );
        r.register(updated);
        let cfg = r.get("google").unwrap();
        assert_eq!(cfg.client_id, "new-client");
    }

    #[test]
    fn list_returns_registered_ids() {
        let mut r = OAuthRegistry::new();
        r.register(google());
        r.register(OAuthProviderConfig::new(
            "github",
            "c",
            "s",
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "https://app.autome/oauth/callback/github",
        ));
        let mut ids: Vec<&str> = r.list();
        ids.sort();
        assert_eq!(ids, vec!["github", "google"]);
    }

    #[test]
    fn fresh_registry_is_empty() {
        assert!(OAuthRegistry::new().is_empty());
    }
}

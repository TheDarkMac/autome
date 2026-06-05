//! Configuration et registre des providers OAuth2 (F6.2.c).
//!
//! [`OAuthProviderConfig`] décrit un provider (Google, Slack, Notion, GitHub,
//! …). [`OAuthRegistry`] en indexe plusieurs par `provider_id` — la caller
//! passe `provider_id` aux helpers du module pour dispatcher.

use std::collections::HashMap;
use std::fmt;

use autome_core::{Error, Result};

/// Préfixe des variables d'environnement de config OAuth.
const OAUTH_ENV_PREFIX: &str = "AUTOME_OAUTH_";

/// Configuration d'un provider OAuth2.
///
/// `Debug` est **redacté** sur `client_secret` (cohérent avec la règle
/// « secrets jamais en clair » — ARCHITECTURE.md §8).
#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for OAuthProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `client_secret` redacté : ni la valeur ni sa longueur ne fuitent.
        f.debug_struct("OAuthProviderConfig")
            .field("provider_id", &self.provider_id)
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .finish()
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

    /// Charge un registre depuis l'environnement du processus (dette #107,
    /// constat 3 / #161). Voir [`OAuthRegistry::from_source`].
    pub fn from_env() -> Result<Self> {
        Self::from_source(|k| std::env::var(k).ok())
    }

    /// Charge un registre depuis une source clé→valeur arbitraire — injectable,
    /// donc testable sans toucher l'environnement global (même pattern que
    /// [`autome_core::Config::load`]).
    ///
    /// `AUTOME_OAUTH_PROVIDERS` liste les `provider_id` (séparés par virgule).
    /// Pour chaque `<ID>` (mis en majuscules, `-` → `_`), les variables sont
    /// lues sous le préfixe `AUTOME_OAUTH_<ID>_…` :
    /// - **requises** : `CLIENT_ID`, `CLIENT_SECRET`, `AUTHORIZE_URL`,
    ///   `TOKEN_URL`, `REDIRECT_URI` ;
    /// - **optionnelle** : `SCOPES` (séparés par virgule ou espaces).
    ///
    /// Liste absente/vide ⇒ registre vide (défaut dev, OAuth indisponible).
    /// Une variable **requise absente** pour un provider listé est une
    /// **erreur** (fail-fast au démarrage, cohérent avec la validation prod
    /// de `Config`).
    pub fn from_source(src: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut registry = Self::new();
        let Some(listed) = src(&format!("{OAUTH_ENV_PREFIX}PROVIDERS")) else {
            return Ok(registry);
        };

        for raw in listed.split(',') {
            let provider_id = raw.trim();
            if provider_id.is_empty() {
                continue;
            }
            let env_id = provider_id.to_ascii_uppercase().replace('-', "_");

            // Variable non vide après trim, sinon `None`.
            let var = |suffix: &str| -> Option<String> {
                src(&format!("{OAUTH_ENV_PREFIX}{env_id}_{suffix}"))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            let required = |suffix: &str| -> Result<String> {
                var(suffix).ok_or_else(|| {
                    Error::Invalid(format!(
                        "oauth : {OAUTH_ENV_PREFIX}{env_id}_{suffix} requis pour le provider '{provider_id}'"
                    ))
                })
            };

            let mut config = OAuthProviderConfig::new(
                provider_id,
                required("CLIENT_ID")?,
                required("CLIENT_SECRET")?,
                required("AUTHORIZE_URL")?,
                required("TOKEN_URL")?,
                required("REDIRECT_URI")?,
            );
            if let Some(scopes) = var("SCOPES") {
                let parsed: Vec<&str> = scopes
                    .split([',', ' ', '\t', '\n'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                config = config.with_scopes(parsed);
            }
            registry.register(config);
        }
        Ok(registry)
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

    // ───────────────── #161 — chargement depuis l'environnement ─────────────

    fn src(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn from_source_without_providers_var_is_empty() {
        // Absence totale de config OAuth ⇒ registre vide (défaut dev), pas une
        // erreur : l'OAuth est simplement indisponible.
        assert!(OAuthRegistry::from_source(src(&[])).unwrap().is_empty());
        // Liste vide / virgules seules ⇒ idem.
        assert!(
            OAuthRegistry::from_source(src(&[("AUTOME_OAUTH_PROVIDERS", " , ,")]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn from_source_loads_listed_providers_with_required_fields() {
        let r = OAuthRegistry::from_source(src(&[
            ("AUTOME_OAUTH_PROVIDERS", "google, github"),
            ("AUTOME_OAUTH_GOOGLE_CLIENT_ID", "gid"),
            ("AUTOME_OAUTH_GOOGLE_CLIENT_SECRET", "gsecret"),
            (
                "AUTOME_OAUTH_GOOGLE_AUTHORIZE_URL",
                "https://accounts.google.com/o/oauth2/v2/auth",
            ),
            (
                "AUTOME_OAUTH_GOOGLE_TOKEN_URL",
                "https://oauth2.googleapis.com/token",
            ),
            (
                "AUTOME_OAUTH_GOOGLE_REDIRECT_URI",
                "https://app.autome/oauth/callback/google",
            ),
            ("AUTOME_OAUTH_GOOGLE_SCOPES", "openid, email profile"),
            ("AUTOME_OAUTH_GITHUB_CLIENT_ID", "ghid"),
            ("AUTOME_OAUTH_GITHUB_CLIENT_SECRET", "ghsecret"),
            (
                "AUTOME_OAUTH_GITHUB_AUTHORIZE_URL",
                "https://github.com/login/oauth/authorize",
            ),
            (
                "AUTOME_OAUTH_GITHUB_TOKEN_URL",
                "https://github.com/login/oauth/access_token",
            ),
            (
                "AUTOME_OAUTH_GITHUB_REDIRECT_URI",
                "https://app.autome/oauth/callback/github",
            ),
        ]))
        .unwrap();

        let mut ids = r.list();
        ids.sort();
        assert_eq!(ids, vec!["github", "google"]);

        let g = r.get("google").unwrap();
        assert_eq!(g.client_id, "gid");
        assert_eq!(g.client_secret, "gsecret");
        assert_eq!(g.token_url, "https://oauth2.googleapis.com/token");
        assert_eq!(g.redirect_uri, "https://app.autome/oauth/callback/google");
        // Scopes séparés par virgule **ou** espace.
        assert_eq!(g.scopes, vec!["openid", "email", "profile"]);

        // GitHub : pas de SCOPES → liste vide (optionnel).
        assert!(r.get("github").unwrap().scopes.is_empty());
    }

    #[test]
    fn from_source_missing_required_field_is_an_error() {
        // `google` listé mais sans `CLIENT_SECRET` → fail-fast nommant la var.
        let err = OAuthRegistry::from_source(src(&[
            ("AUTOME_OAUTH_PROVIDERS", "google"),
            ("AUTOME_OAUTH_GOOGLE_CLIENT_ID", "gid"),
            ("AUTOME_OAUTH_GOOGLE_AUTHORIZE_URL", "https://x/auth"),
            ("AUTOME_OAUTH_GOOGLE_TOKEN_URL", "https://x/token"),
            ("AUTOME_OAUTH_GOOGLE_REDIRECT_URI", "https://x/cb"),
        ]))
        .unwrap_err();
        assert!(
            matches!(err, Error::Invalid(ref m)
                if m.contains("AUTOME_OAUTH_GOOGLE_CLIENT_SECRET") && m.contains("google")),
            "message inattendu : {err:?}"
        );
    }

    #[test]
    fn from_source_uppercases_and_normalizes_hyphen_in_id() {
        // `provider_id` avec tiret → variables en `AZURE_AD` (majuscule, `_`).
        let r = OAuthRegistry::from_source(src(&[
            ("AUTOME_OAUTH_PROVIDERS", "azure-ad"),
            ("AUTOME_OAUTH_AZURE_AD_CLIENT_ID", "aid"),
            ("AUTOME_OAUTH_AZURE_AD_CLIENT_SECRET", "asecret"),
            (
                "AUTOME_OAUTH_AZURE_AD_AUTHORIZE_URL",
                "https://login/authorize",
            ),
            ("AUTOME_OAUTH_AZURE_AD_TOKEN_URL", "https://login/token"),
            ("AUTOME_OAUTH_AZURE_AD_REDIRECT_URI", "https://app/cb"),
        ]))
        .unwrap();
        // Le `provider_id` conserve sa forme d'origine (tiret).
        assert_eq!(r.get("azure-ad").unwrap().client_id, "aid");
    }

    #[test]
    fn debug_redacts_client_secret() {
        let cfg = google();
        let dump = format!("{cfg:?}");
        assert!(dump.contains("client_secret"));
        assert!(dump.contains("***"));
        assert!(
            !dump.contains("secret-google"),
            "le client_secret ne doit jamais fuiter en Debug : {dump}"
        );
    }
}

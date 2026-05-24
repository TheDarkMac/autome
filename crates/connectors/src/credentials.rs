//! Service de gestion des credentials connecteurs (F7.3.a).
//!
//! Ce module orchestre le `OAuthClient` (F6.2.a) et le
//! `ConnectorTokenStore` (F6.2.b) derrière un trait étroit
//! [`CredentialService`] que l'API publique consomme via
//! `Arc<dyn CredentialService + Send + Sync>`. Le trait expose les cinq
//! opérations dont a besoin un caller HTTP :
//!
//! 1. [`list_providers`](CredentialService::list_providers) — les providers
//!    déclarés (sans le `client_secret`).
//! 2. [`start_authorize`](CredentialService::start_authorize) — génère
//!    l'URL d'autorisation + PKCE + state.
//! 3. [`complete_authorize`](CredentialService::complete_authorize) —
//!    échange le `code` contre une `TokenSet` et la persiste chiffrée
//!    pour `(user_id, provider_id)`.
//! 4. [`list_for_user`](CredentialService::list_for_user) — résumé des
//!    credentials de l'utilisateur courant (jamais le token brut).
//! 5. [`delete`](CredentialService::delete) — révoque un credential.
//!
//! L'impl [`OAuthCredentialService`] est paramétrée par le `HttpClient`
//! utilisé sous le `OAuthClient` ; les tests api utilisent un mock qui
//! implémente directement le trait sans dépendre du transport HTTP.

use std::sync::Arc;

use autome_core::Result;

use crate::http::HttpClient;
use crate::oauth::client::{AuthorizationStart, OAuthClient, TokenSet};
use crate::oauth::store::ConnectorTokenStore;

/// Description publique d'un provider OAuth — ne fuit aucun secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    /// Identifiant stable du provider (`"google"`, `"github"`, …).
    pub provider_id: String,
    /// Scopes que le provider demandera.
    pub scopes: Vec<String>,
}

/// Vue d'un credential stocké, safe à exposer en HTTP.
///
/// On expose **uniquement** des métadonnées : pas d'`access_token`,
/// pas de `refresh_token` en clair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSummary {
    /// Provider concerné.
    pub provider_id: String,
    /// `true` si un refresh token est conservé (donc renouvellement
    /// silencieux possible).
    pub has_refresh_token: bool,
    /// Scopes effectivement accordés par le provider (verbatim).
    pub scope: Option<String>,
    /// Timestamp d'expiration de l'access token (Unix ms), si connu.
    pub expires_at_ms: Option<i64>,
}

impl CredentialSummary {
    /// Convertit une `TokenSet` brute en résumé safe à exposer.
    pub fn from_tokens(provider_id: impl Into<String>, tokens: &TokenSet) -> Self {
        Self {
            provider_id: provider_id.into(),
            has_refresh_token: tokens.refresh_token.is_some(),
            scope: tokens.scope.clone(),
            expires_at_ms: tokens.expires_at_ms,
        }
    }
}

/// Contrat consommé par l'API publique (F7.3.b).
///
/// Le trait est object-safe : l'API détient un
/// `Arc<dyn CredentialService + Send + Sync>` afin que les tests puissent
/// injecter un mock sans embarquer un transport HTTP réel.
pub trait CredentialService {
    /// Liste les providers déclarés (jamais le `client_secret`).
    fn list_providers(&self) -> Vec<ProviderInfo>;

    /// Démarre un flow OAuth — renvoie URL d'autorisation + state + PKCE
    /// verifier. Le caller stocke `state` + `code_verifier` côté client
    /// (en attendant le callback).
    fn start_authorize(&self, provider_id: &str) -> Result<AuthorizationStart>;

    /// Échange le `code` reçu en callback contre une `TokenSet` puis la
    /// persiste chiffrée pour `(user_id, provider_id)`. Renvoie le résumé
    /// safe à exposer en HTTP.
    fn complete_authorize(
        &self,
        user_id: &str,
        provider_id: &str,
        code: &str,
        code_verifier: &str,
        now_ms: i64,
    ) -> Result<CredentialSummary>;

    /// Résumé des credentials de `user_id`, tri lexical par `provider_id`.
    fn list_for_user(&self, user_id: &str) -> Result<Vec<CredentialSummary>>;

    /// Révoque un credential. No-op si absent.
    fn delete(&self, user_id: &str, provider_id: &str) -> Result<()>;
}

// ────────────────────── OAuthCredentialService ──────────────────────

/// Impl par défaut — câble `OAuthClient` + `ConnectorTokenStore`.
///
/// Le paramètre `C: HttpClient` est encapsulé : l'API publique manipule
/// l'objet derrière `Arc<dyn CredentialService + Send + Sync>` sans
/// connaître le type concret du transport.
pub struct OAuthCredentialService<C: HttpClient> {
    oauth: OAuthClient<C>,
    store: Arc<dyn ConnectorTokenStore + Send + Sync>,
}

impl<C: HttpClient> OAuthCredentialService<C> {
    /// Construit le service à partir d'un client OAuth déjà configuré
    /// (registre + transport) et d'un store de tokens chiffrés.
    pub fn new(oauth: OAuthClient<C>, store: Arc<dyn ConnectorTokenStore + Send + Sync>) -> Self {
        Self { oauth, store }
    }

    /// Accès au client OAuth sous-jacent (utile en tests).
    pub fn oauth(&self) -> &OAuthClient<C> {
        &self.oauth
    }
}

impl<C: HttpClient> CredentialService for OAuthCredentialService<C> {
    fn list_providers(&self) -> Vec<ProviderInfo> {
        let mut out: Vec<ProviderInfo> = self
            .oauth
            .registry()
            .list()
            .into_iter()
            .filter_map(|pid| {
                self.oauth.registry().get(pid).ok().map(|cfg| ProviderInfo {
                    provider_id: cfg.provider_id.clone(),
                    scopes: cfg.scopes.clone(),
                })
            })
            .collect();
        out.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        out
    }

    fn start_authorize(&self, provider_id: &str) -> Result<AuthorizationStart> {
        self.oauth.start_authorization(provider_id)
    }

    fn complete_authorize(
        &self,
        user_id: &str,
        provider_id: &str,
        code: &str,
        code_verifier: &str,
        now_ms: i64,
    ) -> Result<CredentialSummary> {
        let tokens = self
            .oauth
            .exchange_code(provider_id, code, code_verifier, now_ms)?;
        self.store.store(user_id, provider_id, &tokens)?;
        Ok(CredentialSummary::from_tokens(provider_id, &tokens))
    }

    fn list_for_user(&self, user_id: &str) -> Result<Vec<CredentialSummary>> {
        let provider_ids = self.store.list_by_user(user_id)?;
        let mut out = Vec::with_capacity(provider_ids.len());
        for pid in provider_ids {
            if let Some(tokens) = self.store.load(user_id, &pid)? {
                out.push(CredentialSummary::from_tokens(pid, &tokens));
            }
        }
        Ok(out)
    }

    fn delete(&self, user_id: &str, provider_id: &str) -> Result<()> {
        self.store.delete(user_id, provider_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{EnvelopeCipher, InMemoryKeyProvider};
    use serde_json::json;

    use crate::oauth::provider::{OAuthProviderConfig, OAuthRegistry};
    use crate::oauth::store::InMemoryConnectorTokenStore;
    use crate::sdk::RecordingClient;

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

    fn slack() -> OAuthProviderConfig {
        OAuthProviderConfig::new(
            "slack",
            "client-id-slack",
            "secret-slack",
            "https://slack.com/oauth/v2/authorize",
            "https://slack.com/api/oauth.v2.access",
            "https://app.autome/oauth/callback/slack",
        )
        .with_scopes(["chat:write", "channels:read"])
    }

    fn registry_with(providers: Vec<OAuthProviderConfig>) -> OAuthRegistry {
        let mut r = OAuthRegistry::new();
        for p in providers {
            r.register(p);
        }
        r
    }

    fn make_store() -> Arc<InMemoryConnectorTokenStore<InMemoryKeyProvider>> {
        let provider = InMemoryKeyProvider::with_key("k1", [7u8; 32]);
        let cipher = EnvelopeCipher::new(provider);
        Arc::new(InMemoryConnectorTokenStore::new(cipher))
    }

    fn make_service(
        registry: OAuthRegistry,
        transport: RecordingClient,
        store: Arc<InMemoryConnectorTokenStore<InMemoryKeyProvider>>,
    ) -> OAuthCredentialService<RecordingClient> {
        let oauth = OAuthClient::new(registry, transport);
        OAuthCredentialService::new(oauth, store as Arc<dyn ConnectorTokenStore + Send + Sync>)
    }

    #[test]
    fn list_providers_exposes_scopes_and_is_sorted_lexically() {
        let svc = make_service(
            registry_with(vec![slack(), google()]),
            RecordingClient::ok(json!({})),
            make_store(),
        );
        let providers = svc.list_providers();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].provider_id, "google");
        assert_eq!(providers[0].scopes, vec!["openid", "email", "profile"]);
        assert_eq!(providers[1].provider_id, "slack");
        assert_eq!(providers[1].scopes, vec!["chat:write", "channels:read"]);
    }

    #[test]
    fn list_providers_never_returns_client_secret_field() {
        // ProviderInfo n'expose pas de champ secret — vérifié par construction
        // (la struct n'a que provider_id + scopes). Test sentinelle pour que
        // toute future addition de champ déclenche une revue.
        let svc = make_service(
            registry_with(vec![google()]),
            RecordingClient::ok(json!({})),
            make_store(),
        );
        let info = svc.list_providers().into_iter().next().unwrap();
        let dbg = format!("{info:?}");
        assert!(
            !dbg.contains("secret-google"),
            "client_secret ne doit jamais apparaître : {dbg}"
        );
    }

    #[test]
    fn start_authorize_returns_url_state_and_verifier() {
        let svc = make_service(
            registry_with(vec![google()]),
            RecordingClient::ok(json!({})),
            make_store(),
        );
        let start = svc.start_authorize("google").unwrap();
        assert!(
            start
                .authorize_url
                .starts_with("https://accounts.google.com/"),
            "authorize_url = {}",
            start.authorize_url
        );
        assert!(!start.state.is_empty());
        assert!(!start.code_verifier.is_empty());
    }

    #[test]
    fn start_authorize_fails_for_unknown_provider() {
        let svc = make_service(
            registry_with(vec![google()]),
            RecordingClient::ok(json!({})),
            make_store(),
        );
        let err = svc.start_authorize("dropbox").unwrap_err();
        assert!(
            format!("{err}").contains("dropbox"),
            "erreur doit citer le provider : {err}"
        );
    }

    #[test]
    fn complete_authorize_exchanges_code_and_persists_tokens_encrypted() {
        let store = make_store();
        let svc = make_service(
            registry_with(vec![google()]),
            RecordingClient::ok(json!({
                "access_token": "AT-google-1",
                "refresh_token": "RT-google-1",
                "expires_in": 3600,
                "token_type": "Bearer",
                "scope": "openid email"
            })),
            Arc::clone(&store),
        );
        let summary = svc
            .complete_authorize(
                "alice",
                "google",
                "auth-code",
                "verifier",
                1_700_000_000_000,
            )
            .unwrap();
        assert_eq!(summary.provider_id, "google");
        assert!(summary.has_refresh_token);
        assert_eq!(summary.scope.as_deref(), Some("openid email"));
        assert_eq!(summary.expires_at_ms, Some(1_700_000_000_000 + 3_600_000));

        // Persistance vérifiée — le token est rechargeable.
        let reloaded = store.load("alice", "google").unwrap().unwrap();
        assert_eq!(reloaded.access_token, "AT-google-1");
        assert_eq!(reloaded.refresh_token.as_deref(), Some("RT-google-1"));
    }

    #[test]
    fn complete_authorize_returns_summary_without_refresh_when_provider_omits_it() {
        let store = make_store();
        let svc = make_service(
            registry_with(vec![google()]),
            RecordingClient::ok(json!({
                "access_token": "AT-2",
                "expires_in": 60,
                "token_type": "Bearer"
            })),
            Arc::clone(&store),
        );
        let summary = svc
            .complete_authorize("alice", "google", "c", "v", 0)
            .unwrap();
        assert!(!summary.has_refresh_token);
        assert!(summary.scope.is_none());
    }

    #[test]
    fn complete_authorize_propagates_http_error_and_does_not_store_anything() {
        let store = make_store();
        let svc = make_service(
            registry_with(vec![google()]),
            RecordingClient::with_status(401, json!({"error": "invalid_grant"})),
            Arc::clone(&store),
        );
        let err = svc
            .complete_authorize("alice", "google", "bad-code", "v", 0)
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("http"));
        assert!(
            store.load("alice", "google").unwrap().is_none(),
            "rien ne doit être persisté quand l'échange échoue"
        );
    }

    #[test]
    fn list_for_user_returns_summaries_sorted_by_provider() {
        let store = make_store();
        let svc = make_service(
            registry_with(vec![google(), slack()]),
            RecordingClient::queue(vec![
                Ok(crate::http::HttpResponse {
                    status: 200,
                    body: json!({
                        "access_token": "AT-slack", "expires_in": 60,
                        "token_type": "Bearer", "scope": "chat:write"
                    }),
                }),
                Ok(crate::http::HttpResponse {
                    status: 200,
                    body: json!({
                        "access_token": "AT-google", "refresh_token": "RT",
                        "expires_in": 60, "token_type": "Bearer", "scope": "openid"
                    }),
                }),
            ]),
            Arc::clone(&store),
        );
        svc.complete_authorize("alice", "slack", "c1", "v1", 1_000)
            .unwrap();
        svc.complete_authorize("alice", "google", "c2", "v2", 1_000)
            .unwrap();

        let listed = svc.list_for_user("alice").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].provider_id, "google");
        assert!(listed[0].has_refresh_token);
        assert_eq!(listed[1].provider_id, "slack");
        assert!(!listed[1].has_refresh_token);
    }

    #[test]
    fn list_for_user_is_isolated_between_users() {
        let store = make_store();
        let svc = make_service(
            registry_with(vec![google()]),
            RecordingClient::ok(json!({
                "access_token": "AT", "expires_in": 60, "token_type": "Bearer"
            })),
            Arc::clone(&store),
        );
        svc.complete_authorize("alice", "google", "c", "v", 0)
            .unwrap();
        assert_eq!(svc.list_for_user("alice").unwrap().len(), 1);
        assert!(svc.list_for_user("bob").unwrap().is_empty());
    }

    #[test]
    fn delete_removes_only_the_targeted_pair() {
        let store = make_store();
        let svc = make_service(
            registry_with(vec![google(), slack()]),
            RecordingClient::queue(vec![
                Ok(crate::http::HttpResponse {
                    status: 200,
                    body: json!({"access_token": "g", "expires_in": 60, "token_type": "Bearer"}),
                }),
                Ok(crate::http::HttpResponse {
                    status: 200,
                    body: json!({"access_token": "s", "expires_in": 60, "token_type": "Bearer"}),
                }),
            ]),
            Arc::clone(&store),
        );
        svc.complete_authorize("alice", "google", "c", "v", 0)
            .unwrap();
        svc.complete_authorize("alice", "slack", "c", "v", 0)
            .unwrap();
        svc.delete("alice", "google").unwrap();
        let remaining = svc.list_for_user("alice").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].provider_id, "slack");
    }

    #[test]
    fn delete_unknown_is_noop() {
        let svc = make_service(
            registry_with(vec![google()]),
            RecordingClient::ok(json!({})),
            make_store(),
        );
        svc.delete("ghost", "google").unwrap();
    }
}

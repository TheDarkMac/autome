//! Client OAuth2 — Authorization Code + PKCE + refresh (F6.2.a + F6.2.b).
//!
//! Le client est paramétré par un [`HttpClient`] (seam habituel — `RecordingClient`
//! en tests, transport réel ailleurs). Trois opérations exposées :
//!
//! - [`OAuthClient::start_authorization`] : URL d'autorisation + PKCE + state.
//! - [`OAuthClient::exchange_code`] : code → `TokenSet`.
//! - [`OAuthClient::refresh`] : refresh_token → nouveau `TokenSet`.
//!
//! Le **format des requêtes token** est `application/x-www-form-urlencoded`
//! (RFC 6749 §4.1.3 et §6). Comme notre [`HttpRequest`] expose `body: Value`,
//! on emballe le form en `Value::String(...)` et on positionne le bon
//! `Content-Type` ; le client HTTP réel détectera la chaîne via le header
//! et la transmettra en bytes verbatim.

use std::collections::BTreeMap;

use autome_core::{Error, Result};
use serde_json::Value;

use crate::http::{HttpClient, HttpRequest};
use crate::oauth::pkce::{generate_pkce, generate_state, PkcePair};
use crate::oauth::provider::{OAuthProviderConfig, OAuthRegistry};
use crate::oauth::refresh::{is_revoked_refresh, is_token_error, RefreshError};
use crate::sdk::http::to_query;

/// Données à conserver côté serveur après [`OAuthClient::start_authorization`]
/// jusqu'au callback du provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationStart {
    /// URL absolue où rediriger l'utilisateur.
    pub authorize_url: String,
    /// Paramètre `state` — doit être présent identique dans le callback (CSRF).
    pub state: String,
    /// PKCE verifier — à présenter à [`OAuthClient::exchange_code`].
    pub code_verifier: String,
}

/// Bundle de tokens reçu d'un endpoint `token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSet {
    /// Access token courant (à présenter aux APIs du provider).
    pub access_token: String,
    /// Refresh token (`None` si le provider n'en émet pas — ex. flow sans
    /// `offline_access`).
    pub refresh_token: Option<String>,
    /// Timestamp Unix (ms) à partir duquel `access_token` n'est plus valide.
    /// Calculé par le client à partir de `expires_in` + `now_ms`.
    pub expires_at_ms: Option<i64>,
    /// Type de token (en général `"Bearer"`).
    pub token_type: String,
    /// Scopes effectivement accordés (chaîne séparée par espaces).
    pub scope: Option<String>,
}

/// Client OAuth2 — wraps un [`HttpClient`].
pub struct OAuthClient<C: HttpClient> {
    registry: OAuthRegistry,
    transport: C,
}

impl<C: HttpClient> OAuthClient<C> {
    /// Construit le client.
    pub fn new(registry: OAuthRegistry, transport: C) -> Self {
        Self {
            registry,
            transport,
        }
    }

    /// Accès au registre (lecture).
    pub fn registry(&self) -> &OAuthRegistry {
        &self.registry
    }

    /// Accès au transport (utile en tests).
    pub fn transport(&self) -> &C {
        &self.transport
    }

    /// Étape 1 — construit l'URL d'autorisation + PKCE + state.
    pub fn start_authorization(&self, provider_id: &str) -> Result<AuthorizationStart> {
        let provider = self.registry.get(provider_id)?;
        let pkce = generate_pkce();
        let state = generate_state();
        let authorize_url = build_authorize_url(provider, &pkce, &state);
        Ok(AuthorizationStart {
            authorize_url,
            state,
            code_verifier: pkce.verifier,
        })
    }

    /// Étape 3 — échange `code` contre une `TokenSet`. `now_ms` est utilisé
    /// pour calculer `expires_at_ms` à partir de `expires_in`.
    pub fn exchange_code(
        &self,
        provider_id: &str,
        code: &str,
        code_verifier: &str,
        now_ms: i64,
    ) -> Result<TokenSet> {
        let provider = self.registry.get(provider_id)?;
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", provider.client_id.as_str()),
            ("client_secret", provider.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", provider.redirect_uri.as_str()),
            ("code_verifier", code_verifier),
        ];
        let req = build_token_request(&provider.token_url, &form);
        let resp = self
            .transport
            .send(&req)
            .map_err(|e| Error::Invalid(format!("oauth exchange : transport {e}")))?;
        // Slack (`ok: false`) et GitHub signalent une erreur en HTTP 200.
        if is_token_error(resp.status, &resp.body) {
            return Err(Error::Invalid(format!(
                "oauth exchange : http {} — {}",
                resp.status, resp.body
            )));
        }
        parse_token_response(&resp.body, now_ms)
    }

    /// Refresh — échange `refresh_token` contre une nouvelle `TokenSet`.
    /// Caller doit re-stocker via [`crate::oauth::ConnectorTokenStore::store`].
    pub fn refresh(&self, provider_id: &str, refresh_token: &str, now_ms: i64) -> Result<TokenSet> {
        Ok(self.try_refresh(provider_id, refresh_token, now_ms)?)
    }

    /// Comme [`refresh`](Self::refresh), en distinguant le refus définitif du
    /// refresh token (`invalid_grant` ou équivalent, même en HTTP 200 ⇒
    /// [`RefreshError::Revoked`]) des échecs transitoires ou de configuration
    /// ([`RefreshError::Failed`]).
    pub fn try_refresh(
        &self,
        provider_id: &str,
        refresh_token: &str,
        now_ms: i64,
    ) -> std::result::Result<TokenSet, RefreshError> {
        let provider = self
            .registry
            .get(provider_id)
            .map_err(|e| RefreshError::Failed(e.to_string()))?;
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", provider.client_id.as_str()),
            ("client_secret", provider.client_secret.as_str()),
            ("refresh_token", refresh_token),
        ];
        let req = build_token_request(&provider.token_url, &form);
        let resp = self
            .transport
            .send(&req)
            .map_err(|e| RefreshError::Failed(format!("oauth refresh : transport {e}")))?;
        if is_token_error(resp.status, &resp.body) {
            let message = format!("oauth refresh : http {} — {}", resp.status, resp.body);
            return Err(if is_revoked_refresh(resp.status, &resp.body) {
                RefreshError::Revoked(message)
            } else {
                RefreshError::Failed(message)
            });
        }
        parse_token_response(&resp.body, now_ms).map_err(|e| RefreshError::Failed(e.to_string()))
    }
}

// ───────────────────────── helpers internes ─────────────────────────

fn build_authorize_url(provider: &OAuthProviderConfig, pkce: &PkcePair, state: &str) -> String {
    // Ordre fixe pour reproductibilité des tests.
    let scope = provider.scopes.join(" ");
    let pairs = [
        ("client_id", provider.client_id.as_str()),
        ("redirect_uri", provider.redirect_uri.as_str()),
        ("response_type", "code"),
        ("scope", scope.as_str()),
        ("state", state),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    format!("{}?{}", provider.authorize_url, to_query(&pairs))
}

fn build_token_request(token_url: &str, form: &[(&str, &str)]) -> HttpRequest {
    let body = to_query(form);
    let mut headers = BTreeMap::new();
    headers.insert(
        "Content-Type".into(),
        "application/x-www-form-urlencoded".into(),
    );
    headers.insert("Accept".into(), "application/json".into());
    HttpRequest {
        method: "POST".into(),
        url: token_url.to_string(),
        headers,
        body: Some(Value::String(body)),
    }
}

fn parse_token_response(body: &Value, now_ms: i64) -> Result<TokenSet> {
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("oauth : `access_token` manquant".into()))?
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    let token_type = body
        .get("token_type")
        .and_then(Value::as_str)
        .unwrap_or("Bearer")
        .to_string();
    let scope = body
        .get("scope")
        .and_then(Value::as_str)
        .map(str::to_string);
    let expires_at_ms = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .map(|s| now_ms.saturating_add(s.saturating_mul(1_000)));
    Ok(TokenSet {
        access_token,
        refresh_token,
        expires_at_ms,
        token_type,
        scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::provider::OAuthProviderConfig;
    use crate::sdk::RecordingClient;
    use serde_json::json;

    const NOW_MS: i64 = 1_700_000_000_000;

    fn google_provider() -> OAuthProviderConfig {
        OAuthProviderConfig::new(
            "google",
            "client-id-g",
            "secret-g",
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
            "https://app/cb",
        )
        .with_scopes(["openid", "email"])
    }

    fn registry_with_google() -> OAuthRegistry {
        let mut r = OAuthRegistry::new();
        r.register(google_provider());
        r
    }

    /// Slack et GitHub, qui signalent leurs erreurs en HTTP 200.
    fn registry_with_slack_and_github() -> OAuthRegistry {
        let mut r = OAuthRegistry::new();
        r.register(OAuthProviderConfig::new(
            "slack",
            "client-id-s",
            "secret-s",
            "https://slack.com/oauth/v2/authorize",
            "https://slack.com/api/oauth.v2.access",
            "https://app/cb",
        ));
        r.register(OAuthProviderConfig::new(
            "github",
            "client-id-gh",
            "secret-gh",
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "https://app/cb",
        ));
        r
    }

    // ───── F6.2.a — start_authorization ─────

    #[test]
    fn start_authorization_builds_url_with_all_params() {
        let client = OAuthClient::new(registry_with_google(), RecordingClient::ok(json!({})));
        let start = client.start_authorization("google").unwrap();
        let url = &start.authorize_url;

        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        // Tous les paramètres attendus sont présents et correctement encodés
        assert!(url.contains("client_id=client-id-g"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fapp%2Fcb"));
        assert!(url.contains("response_type=code"));
        // scope avec espace → %20
        assert!(url.contains("scope=openid%20email"));
        // state et code_challenge présents
        assert!(url.contains(&format!("state={}", start.state)));
        // code_challenge_method=S256
        assert!(url.contains("code_challenge_method=S256"));
        // PKCE verifier conservé pour la suite
        assert_eq!(start.code_verifier.len(), 43);
    }

    #[test]
    fn start_authorization_unknown_provider_errors() {
        let client = OAuthClient::new(OAuthRegistry::new(), RecordingClient::ok(json!({})));
        let err = client.start_authorization("missing").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    #[test]
    fn start_authorization_produces_fresh_state_and_pkce() {
        let client = OAuthClient::new(registry_with_google(), RecordingClient::ok(json!({})));
        let a = client.start_authorization("google").unwrap();
        let b = client.start_authorization("google").unwrap();
        assert_ne!(a.state, b.state);
        assert_ne!(a.code_verifier, b.code_verifier);
    }

    // ───── F6.2.a — exchange_code ─────

    #[test]
    fn exchange_code_posts_form_with_required_fields() {
        let response_body = json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "openid email"
        });
        let client = OAuthClient::new(registry_with_google(), RecordingClient::ok(response_body));
        let token = client
            .exchange_code("google", "code-abc", "verifier-xyz", NOW_MS)
            .unwrap();

        // Vérifie la requête
        let sent = client.transport().last().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.url, "https://oauth2.googleapis.com/token");
        assert_eq!(
            sent.headers.get("Content-Type").map(String::as_str),
            Some("application/x-www-form-urlencoded")
        );
        let body_str = sent.body.as_ref().unwrap().as_str().unwrap();
        assert!(body_str.contains("grant_type=authorization_code"));
        assert!(body_str.contains("client_id=client-id-g"));
        assert!(body_str.contains("client_secret=secret-g"));
        assert!(body_str.contains("code=code-abc"));
        assert!(body_str.contains("code_verifier=verifier-xyz"));
        assert!(body_str.contains("redirect_uri=https%3A%2F%2Fapp%2Fcb"));

        // Vérifie le TokenSet parsé
        assert_eq!(token.access_token, "at-1");
        assert_eq!(token.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(token.token_type, "Bearer");
        assert_eq!(token.scope.as_deref(), Some("openid email"));
        assert_eq!(token.expires_at_ms, Some(NOW_MS + 3_600_000));
    }

    #[test]
    fn exchange_code_handles_missing_refresh_token() {
        let response_body = json!({
            "access_token": "at-only",
            "expires_in": 3600,
            "token_type": "Bearer"
        });
        let client = OAuthClient::new(registry_with_google(), RecordingClient::ok(response_body));
        let token = client.exchange_code("google", "c", "v", NOW_MS).unwrap();
        assert_eq!(token.access_token, "at-only");
        assert_eq!(token.refresh_token, None);
        assert_eq!(token.scope, None);
    }

    #[test]
    fn exchange_code_rejects_response_without_access_token() {
        let response_body = json!({ "expires_in": 3600 });
        let client = OAuthClient::new(registry_with_google(), RecordingClient::ok(response_body));
        let err = client
            .exchange_code("google", "c", "v", NOW_MS)
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("access_token")));
    }

    #[test]
    fn exchange_code_propagates_http_errors() {
        let client = OAuthClient::new(
            registry_with_google(),
            RecordingClient::with_status(400, json!({ "error": "invalid_grant" })),
        );
        let err = client
            .exchange_code("google", "c", "v", NOW_MS)
            .unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("http 400"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn exchange_code_reports_provider_error_sent_in_http_200() {
        let client = OAuthClient::new(
            registry_with_slack_and_github(),
            RecordingClient::ok(json!({ "ok": false, "error": "invalid_code" })),
        );
        match client.exchange_code("slack", "c", "v", NOW_MS).unwrap_err() {
            Error::Invalid(m) => {
                assert!(
                    m.contains("http 200") && m.contains("invalid_code"),
                    "msg={m}"
                )
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn exchange_code_unknown_provider_errors() {
        let client = OAuthClient::new(OAuthRegistry::new(), RecordingClient::ok(json!({})));
        let err = client.exchange_code("nope", "c", "v", NOW_MS).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    // ───── F6.2.b — refresh ─────

    #[test]
    fn refresh_posts_form_with_refresh_grant() {
        let response_body = json!({
            "access_token": "at-2",
            "refresh_token": "rt-2",
            "expires_in": 1800,
            "token_type": "Bearer"
        });
        let client = OAuthClient::new(registry_with_google(), RecordingClient::ok(response_body));
        let token = client.refresh("google", "rt-old", NOW_MS).unwrap();

        let sent = client.transport().last().unwrap();
        let body = sent.body.as_ref().unwrap().as_str().unwrap();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("client_id=client-id-g"));
        assert!(body.contains("refresh_token=rt-old"));
        assert!(!body.contains("code="), "pas de code dans un refresh");

        assert_eq!(token.access_token, "at-2");
        assert_eq!(token.refresh_token.as_deref(), Some("rt-2"));
        assert_eq!(token.expires_at_ms, Some(NOW_MS + 1_800_000));
    }

    #[test]
    fn refresh_preserves_refresh_token_when_provider_omits_it() {
        // Certains providers (Slack) ne renvoient pas de nouveau refresh_token
        // au refresh ; le caller doit conserver l'ancien. La parsing ne crée
        // pas de refresh_token artificiel.
        let response_body = json!({
            "access_token": "at-renewed",
            "expires_in": 3600,
            "token_type": "Bearer"
        });
        let client = OAuthClient::new(registry_with_google(), RecordingClient::ok(response_body));
        let token = client.refresh("google", "rt-existing", NOW_MS).unwrap();
        assert_eq!(token.refresh_token, None, "le caller décide quoi en faire");
    }

    #[test]
    fn try_refresh_classifies_invalid_grant_as_revoked() {
        let client = OAuthClient::new(
            registry_with_google(),
            RecordingClient::with_status(
                400,
                json!({ "error": "invalid_grant", "error_description": "Token has been revoked." }),
            ),
        );
        match client.try_refresh("google", "rt-dead", NOW_MS) {
            Err(RefreshError::Revoked(m)) => {
                assert!(m.contains("http 400") && m.contains("revoked"), "{m}")
            }
            other => panic!("Revoked attendu : {other:?}"),
        }
        // L'API historique garde son erreur `core` (même message).
        let err = client.refresh("google", "rt-dead", NOW_MS).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("invalid_grant")));
    }

    #[test]
    fn try_refresh_treats_server_and_other_client_errors_as_transient() {
        for (status, body) in [
            (503, json!({ "error": "temporarily_unavailable" })),
            (401, json!({ "error": "invalid_client" })),
        ] {
            let client = OAuthClient::new(
                registry_with_google(),
                RecordingClient::with_status(status, body),
            );
            assert!(
                matches!(
                    client.try_refresh("google", "rt", NOW_MS),
                    Err(RefreshError::Failed(_))
                ),
                "status {status}"
            );
        }
    }

    #[test]
    fn try_refresh_classifies_refusals_sent_in_http_200_as_revoked() {
        for (provider, body) in [
            (
                "slack",
                json!({ "ok": false, "error": "invalid_refresh_token" }),
            ),
            ("slack", json!({ "ok": false, "error": "token_revoked" })),
            (
                "github",
                json!({
                    "error": "bad_refresh_token",
                    "error_description": "The refresh token passed is incorrect or expired.",
                }),
            ),
        ] {
            let client =
                OAuthClient::new(registry_with_slack_and_github(), RecordingClient::ok(body));
            match client.try_refresh(provider, "rt-dead", NOW_MS) {
                Err(RefreshError::Revoked(m)) => assert!(m.contains("http 200"), "{m}"),
                other => panic!("{provider} : Revoked attendu : {other:?}"),
            }
        }
    }

    #[test]
    fn try_refresh_keeps_other_errors_sent_in_http_200_transient() {
        // Configuration ou limite de débit : le refresh token reste valable,
        // le code du provider figure dans le message (et non « access_token
        // manquant »).
        for (provider, body, code) in [
            (
                "slack",
                json!({ "ok": false, "error": "invalid_client_id" }),
                "invalid_client_id",
            ),
            (
                "slack",
                json!({ "ok": false, "error": "ratelimited" }),
                "ratelimited",
            ),
            (
                "github",
                json!({ "error": "incorrect_client_credentials" }),
                "incorrect_client_credentials",
            ),
        ] {
            let client =
                OAuthClient::new(registry_with_slack_and_github(), RecordingClient::ok(body));
            match client.try_refresh(provider, "rt", NOW_MS) {
                Err(RefreshError::Failed(m)) => assert!(m.contains(code), "{m}"),
                other => panic!("{provider} : Failed attendu : {other:?}"),
            }
        }
    }

    #[test]
    fn try_refresh_accepts_slack_rotated_bot_token() {
        let client = OAuthClient::new(
            registry_with_slack_and_github(),
            RecordingClient::ok(json!({
                "ok": true,
                "access_token": "xoxe.xoxb-new",
                "refresh_token": "xoxe-1-new",
                "expires_in": 43_200,
                "token_type": "bot",
            })),
        );
        let t = client.try_refresh("slack", "xoxe-1-old", NOW_MS).unwrap();
        assert_eq!(t.access_token, "xoxe.xoxb-new");
        assert_eq!(t.refresh_token.as_deref(), Some("xoxe-1-new"));
        assert_eq!(t.expires_at_ms, Some(NOW_MS + 43_200_000));
    }

    #[test]
    fn refresh_unknown_provider_errors() {
        let client = OAuthClient::new(OAuthRegistry::new(), RecordingClient::ok(json!({})));
        let err = client.refresh("missing", "rt", NOW_MS).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    // ───── parse_token_response (helper interne) ─────

    #[test]
    fn parse_response_defaults_token_type_to_bearer() {
        let body = json!({ "access_token": "x" });
        let t = parse_token_response(&body, NOW_MS).unwrap();
        assert_eq!(t.token_type, "Bearer");
    }

    #[test]
    fn parse_response_without_expires_in_has_none() {
        let body = json!({ "access_token": "x" });
        let t = parse_token_response(&body, NOW_MS).unwrap();
        assert_eq!(t.expires_at_ms, None);
    }
}

//! Rafraîchissement des access tokens OAuth expirés (backlog post-audit).
//!
//! Le worker relit le [`TokenSet`] du propriétaire d'un run ; si l'access
//! token expire (ou va expirer dans [`REFRESH_SKEW_MS`]), il l'échange contre
//! un nouveau via un [`TokenRefresher`] (en production : [`OAuthClient`],
//! `grant_type=refresh_token`), puis re-stocke le résultat fusionné par
//! [`merge_refreshed`].
//!
//! Un refus **définitif** du provider (`invalid_grant` et ses équivalents non
//! standard, [`is_revoked_refresh`]) est distingué d'un échec transitoire
//! ([`RefreshError`]) : le worker marque alors la credential invalide au lieu
//! de retenter à chaque run.

use std::fmt;

use autome_core::Error;
use serde_json::Value;

use crate::http::HttpClient;
use crate::oauth::client::{OAuthClient, TokenSet};

/// Marge avant expiration à partir de laquelle on rafraîchit : un token qui
/// expire dans moins d'une minute expirerait pendant l'exécution du node.
pub const REFRESH_SKEW_MS: i64 = 60_000;

/// Échec d'un rafraîchissement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshError {
    /// Refresh token refusé par le provider (`invalid_grant`, RFC 6749 §5.2,
    /// ou son équivalent non standard, [`REVOKED_REFRESH_CODES`]) : révoqué,
    /// expiré ou déjà consommé. Retenter est inutile — l'utilisateur
    /// doit reconnecter le compte.
    Revoked(String),
    /// Échec transitoire (réseau, 5xx, réponse illisible) ou de configuration
    /// (provider inconnu, client refusé) : le refresh token reste valable.
    Failed(String),
}

impl RefreshError {
    /// Message détaillé (réponse du provider comprise).
    pub fn message(&self) -> &str {
        match self {
            Self::Revoked(m) | Self::Failed(m) => m,
        }
    }
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for RefreshError {}

impl From<RefreshError> for Error {
    fn from(e: RefreshError) -> Self {
        Error::Invalid(e.message().to_string())
    }
}

/// Codes d'erreur du endpoint token qui signifient un refresh token refusé
/// pour de bon. La requête de refresh n'envoie aucun autre token que le
/// refresh token : un « token » révoqué ou expiré ne peut être que lui.
pub const REVOKED_REFRESH_CODES: &[&str] = &[
    // RFC 6749 §5.2 (Google, Notion, …).
    "invalid_grant",
    // Slack (`oauth.v2.access`), en HTTP 200 avec `ok: false`.
    "invalid_refresh_token",
    "token_revoked",
    "token_expired",
    // GitHub (`/login/oauth/access_token`).
    "bad_refresh_token",
];

/// `true` si la réponse du endpoint token est une erreur : statut hors 2xx
/// (RFC 6749 §5.2), `ok: false` (Slack, en HTTP 200) ou `error` sans
/// `access_token` (GitHub, en HTTP 200).
pub fn is_token_error(status: u16, body: &Value) -> bool {
    !(200..300).contains(&status)
        || body.get("ok").and_then(Value::as_bool) == Some(false)
        || (body.get("error").is_some_and(Value::is_string) && body.get("access_token").is_none())
}

/// `true` si la réponse du endpoint token refuse définitivement le refresh
/// token : erreur ([`is_token_error`]) dont le code `error` est l'un des
/// [`REVOKED_REFRESH_CODES`]. Jamais sur un 5xx (panne du provider).
pub fn is_revoked_refresh(status: u16, body: &Value) -> bool {
    status < 500
        && is_token_error(status, body)
        && body
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|code| REVOKED_REFRESH_CODES.contains(&code))
}

/// Échange un refresh token contre un nouveau [`TokenSet`].
///
/// Object-safe : le worker détient un `Arc<dyn TokenRefresher + Send + Sync>`
/// (mock en test, [`OAuthClient`] en production).
pub trait TokenRefresher {
    /// Rafraîchit le token de `provider_id` à partir de `refresh_token`.
    fn refresh(
        &self,
        provider_id: &str,
        refresh_token: &str,
        now_ms: i64,
    ) -> Result<TokenSet, RefreshError>;
}

impl<C: HttpClient> TokenRefresher for OAuthClient<C> {
    fn refresh(
        &self,
        provider_id: &str,
        refresh_token: &str,
        now_ms: i64,
    ) -> Result<TokenSet, RefreshError> {
        self.try_refresh(provider_id, refresh_token, now_ms)
    }
}

/// `true` si `tokens` est expiré ou expire dans moins de `skew_ms`.
/// Un token **sans** `expires_at_ms` (provider qui n'émet pas `expires_in`,
/// ex. token de bot Slack) n'est jamais considéré expiré.
pub fn needs_refresh(tokens: &TokenSet, now_ms: i64, skew_ms: i64) -> bool {
    tokens
        .expires_at_ms
        .is_some_and(|exp| exp <= now_ms.saturating_add(skew_ms))
}

/// Fusionne la réponse de rafraîchissement avec le [`TokenSet`] précédent.
///
/// Beaucoup de providers ne renvoient ni nouveau `refresh_token` (Google,
/// Slack) ni `scope` au refresh : on conserve alors les valeurs précédentes,
/// sans quoi le rafraîchissement suivant deviendrait impossible. Un refresh
/// token **renvoyé** (rotation) remplace l'ancien.
pub fn merge_refreshed(previous: &TokenSet, refreshed: TokenSet) -> TokenSet {
    TokenSet {
        refresh_token: refreshed
            .refresh_token
            .or_else(|| previous.refresh_token.clone()),
        scope: refreshed.scope.or_else(|| previous.scope.clone()),
        ..refreshed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::provider::{OAuthProviderConfig, OAuthRegistry};
    use crate::sdk::RecordingClient;
    use serde_json::json;

    const NOW: i64 = 1_700_000_000_000;

    fn tokens(expires_at_ms: Option<i64>) -> TokenSet {
        TokenSet {
            access_token: "at-old".into(),
            refresh_token: Some("rt-old".into()),
            expires_at_ms,
            token_type: "Bearer".into(),
            scope: Some("gmail.send".into()),
        }
    }

    #[test]
    fn token_without_expiry_never_needs_refresh() {
        assert!(!needs_refresh(&tokens(None), NOW, REFRESH_SKEW_MS));
    }

    #[test]
    fn token_needs_refresh_when_expired_or_within_skew() {
        assert!(needs_refresh(&tokens(Some(NOW - 1)), NOW, REFRESH_SKEW_MS));
        assert!(needs_refresh(&tokens(Some(NOW)), NOW, REFRESH_SKEW_MS));
        assert!(needs_refresh(
            &tokens(Some(NOW + REFRESH_SKEW_MS)),
            NOW,
            REFRESH_SKEW_MS
        ));
        assert!(!needs_refresh(
            &tokens(Some(NOW + REFRESH_SKEW_MS + 1)),
            NOW,
            REFRESH_SKEW_MS
        ));
    }

    #[test]
    fn needs_refresh_does_not_overflow() {
        assert!(needs_refresh(
            &tokens(Some(i64::MAX)),
            i64::MAX,
            REFRESH_SKEW_MS
        ));
    }

    #[test]
    fn merge_keeps_previous_refresh_token_and_scope_when_omitted() {
        let refreshed = TokenSet {
            access_token: "at-new".into(),
            refresh_token: None,
            expires_at_ms: Some(NOW + 3_600_000),
            token_type: "Bearer".into(),
            scope: None,
        };
        let merged = merge_refreshed(&tokens(Some(NOW)), refreshed);
        assert_eq!(merged.access_token, "at-new");
        assert_eq!(merged.expires_at_ms, Some(NOW + 3_600_000));
        assert_eq!(merged.refresh_token.as_deref(), Some("rt-old"));
        assert_eq!(merged.scope.as_deref(), Some("gmail.send"));
    }

    #[test]
    fn merge_takes_rotated_refresh_token() {
        let refreshed = TokenSet {
            access_token: "at-new".into(),
            refresh_token: Some("rt-rotated".into()),
            expires_at_ms: None,
            token_type: "Bearer".into(),
            scope: Some("gmail.readonly".into()),
        };
        let merged = merge_refreshed(&tokens(Some(NOW)), refreshed);
        assert_eq!(merged.refresh_token.as_deref(), Some("rt-rotated"));
        assert_eq!(merged.scope.as_deref(), Some("gmail.readonly"));
    }

    #[test]
    fn oauth_client_is_a_token_refresher() {
        let mut registry = OAuthRegistry::new();
        registry.register(OAuthProviderConfig::new(
            "gmail",
            "cid",
            "secret",
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
            "https://app.autome/oauth/callback/gmail",
        ));
        let client = OAuthClient::new(
            registry,
            RecordingClient::ok(json!({
                "access_token": "at-new", "expires_in": 3600, "token_type": "Bearer"
            })),
        );
        let refresher: &dyn TokenRefresher = &client;
        let refreshed = refresher.refresh("gmail", "rt-old", NOW).unwrap();
        assert_eq!(refreshed.access_token, "at-new");
        assert_eq!(refreshed.expires_at_ms, Some(NOW + 3_600_000));
        assert!(matches!(
            refresher.refresh("unknown", "rt", NOW),
            Err(RefreshError::Failed(_))
        ));
    }

    #[test]
    fn invalid_grant_is_revoked_except_on_server_errors() {
        let body = json!({ "error": "invalid_grant", "error_description": "revoked" });
        assert!(is_revoked_refresh(400, &body));
        assert!(is_revoked_refresh(401, &body));
        assert!(is_revoked_refresh(200, &body), "erreur en 200, sans token");
        assert!(!is_revoked_refresh(500, &body));
        assert!(!is_revoked_refresh(503, &body));
        assert!(!is_revoked_refresh(
            400,
            &json!({ "error": "invalid_client" })
        ));
        assert!(!is_revoked_refresh(400, &json!("invalid_grant")));
    }

    #[test]
    fn slack_refusals_in_http_200_are_revoked() {
        for code in ["invalid_refresh_token", "token_revoked", "token_expired"] {
            let body = json!({ "ok": false, "error": code });
            assert!(is_token_error(200, &body), "{code}");
            assert!(is_revoked_refresh(200, &body), "{code}");
        }
    }

    #[test]
    fn github_bad_refresh_token_is_revoked_in_200_and_400() {
        let body = json!({
            "error": "bad_refresh_token",
            "error_description": "The refresh token passed is incorrect or expired.",
        });
        assert!(is_revoked_refresh(200, &body));
        assert!(is_revoked_refresh(400, &body));
    }

    #[test]
    fn other_errors_in_http_200_are_errors_but_not_revoked() {
        for body in [
            json!({ "ok": false, "error": "invalid_client_id" }),
            json!({ "ok": false, "error": "ratelimited" }),
            json!({ "ok": false }),
            json!({ "error": "incorrect_client_credentials" }),
        ] {
            assert!(is_token_error(200, &body), "{body}");
            assert!(!is_revoked_refresh(200, &body), "{body}");
        }
    }

    #[test]
    fn successful_responses_are_not_errors() {
        for body in [
            json!({ "access_token": "at", "expires_in": 3600 }),
            json!({ "ok": true, "access_token": "xoxe.xoxb-1", "refresh_token": "xoxe-1" }),
            // Un `error` accompagné d'un token n'est pas un refus.
            json!({ "access_token": "at", "error": "invalid_grant" }),
        ] {
            assert!(!is_token_error(200, &body), "{body}");
            assert!(!is_revoked_refresh(200, &body), "{body}");
        }
        // Le statut l'emporte : un 4xx est une erreur même avec un token.
        assert!(is_token_error(400, &json!({ "access_token": "at" })));
    }

    #[test]
    fn refresh_error_converts_to_core_error_with_its_message() {
        let err: Error = RefreshError::Revoked("oauth refresh : http 400".into()).into();
        assert!(matches!(err, Error::Invalid(ref m) if m == "oauth refresh : http 400"));
        assert_eq!(
            RefreshError::Failed("down".into()).to_string(),
            "down",
            "Display = message"
        );
    }
}

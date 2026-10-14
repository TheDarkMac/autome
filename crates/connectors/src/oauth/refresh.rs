//! Rafraîchissement des access tokens OAuth expirés (backlog post-audit).
//!
//! Le worker relit le [`TokenSet`] du propriétaire d'un run ; si l'access
//! token expire (ou va expirer dans [`REFRESH_SKEW_MS`]), il l'échange contre
//! un nouveau via un [`TokenRefresher`] (en production : [`OAuthClient`],
//! `grant_type=refresh_token`), puis re-stocke le résultat fusionné par
//! [`merge_refreshed`].

use autome_core::Result;

use crate::http::HttpClient;
use crate::oauth::client::{OAuthClient, TokenSet};

/// Marge avant expiration à partir de laquelle on rafraîchit : un token qui
/// expire dans moins d'une minute expirerait pendant l'exécution du node.
pub const REFRESH_SKEW_MS: i64 = 60_000;

/// Échange un refresh token contre un nouveau [`TokenSet`].
///
/// Object-safe : le worker détient un `Arc<dyn TokenRefresher + Send + Sync>`
/// (mock en test, [`OAuthClient`] en production).
pub trait TokenRefresher {
    /// Rafraîchit le token de `provider_id` à partir de `refresh_token`.
    fn refresh(&self, provider_id: &str, refresh_token: &str, now_ms: i64) -> Result<TokenSet>;
}

impl<C: HttpClient> TokenRefresher for OAuthClient<C> {
    fn refresh(&self, provider_id: &str, refresh_token: &str, now_ms: i64) -> Result<TokenSet> {
        OAuthClient::refresh(self, provider_id, refresh_token, now_ms)
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
        assert!(refresher.refresh("unknown", "rt", NOW).is_err());
    }
}

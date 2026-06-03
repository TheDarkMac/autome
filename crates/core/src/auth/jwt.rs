//! JWT — émission / vérification / rotation (F6.1.b + F6.1.d).
//!
//! Algorithme **HS256** (HMAC-SHA256) — secret partagé symétrique fourni
//! par la couche supérieure (typiquement [`crate::Config::secrets`] /
//! variable d'env). Suffisant pour un déploiement single-tenant / single-
//! audience. Le passage à RS256/EdDSA (asymétrique, multi-audience)
//! nécessitera une rotation de clé KMS et viendra avec F6.3.
//!
//! Deux types de tokens (champ `kind` dans les claims) :
//!
//! - [`TokenKind::Access`] : court-vif, présenté à chaque appel API.
//! - [`TokenKind::Refresh`] : long-vif, ne sert qu'à
//!   [`TokenService::rotate`] pour obtenir une nouvelle paire `(access, refresh)`.
//!
//! Tous les tokens portent un `jti` (UUID v4-like) consommable par
//! [`crate::RevocationStore`] pour révoquer un token avant son `exp`.
//!
//! ```ignore
//! let svc = TokenService::new(b"shared-secret");
//! let access = svc.issue_access("user-1", 900, 1_700_000_000)?;
//! let claims = svc.verify(&access, 1_700_000_500)?;
//! assert_eq!(claims.sub, "user-1");
//! ```

use jsonwebtoken::{
    decode, encode, errors::ErrorKind, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::auth::revocation::RevocationStore;
use crate::{Error, Result};

/// Type d'un token. Persisté dans la claim `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    /// Token court — présenté aux appels API.
    Access,
    /// Token long — sert uniquement à `rotate`.
    Refresh,
}

/// Claims standard portées par les tokens Autome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// `sub` (RFC 7519) — identifiant utilisateur.
    pub sub: String,
    /// `iat` — émis à (Unix seconds).
    pub iat: i64,
    /// `exp` — expire à (Unix seconds).
    pub exp: i64,
    /// `jti` — identifiant unique du token (cible des révocations).
    pub jti: String,
    /// Type de token (claim custom Autome — non-standard).
    pub kind: TokenKind,
}

/// Génère un `jti` aléatoire (16 octets → 32 hex).
fn fresh_jti() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(32);
    for b in &bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Service d'émission/vérification de tokens.
pub struct TokenService {
    encoding: EncodingKey,
    decoding: DecodingKey,
}

impl TokenService {
    /// Construit le service avec un secret partagé. Le secret doit être
    /// suffisamment long (au moins 256 bits = 32 octets pour HS256).
    pub fn new(secret: &[u8]) -> Self {
        Self {
            encoding: EncodingKey::from_secret(secret),
            decoding: DecodingKey::from_secret(secret),
        }
    }

    fn header() -> Header {
        Header::new(Algorithm::HS256)
    }

    fn issue(&self, kind: TokenKind, sub: &str, ttl_seconds: i64, now: i64) -> Result<String> {
        let claims = Claims {
            sub: sub.to_string(),
            iat: now,
            exp: now + ttl_seconds,
            jti: fresh_jti(),
            kind,
        };
        encode(&Self::header(), &claims, &self.encoding)
            .map_err(|e| Error::Invalid(format!("jwt encode : {e}")))
    }

    /// Émet un access token portant `sub`, valide `ttl_seconds` à partir de `now`.
    pub fn issue_access(&self, sub: &str, ttl_seconds: i64, now: i64) -> Result<String> {
        self.issue(TokenKind::Access, sub, ttl_seconds, now)
    }

    /// Émet un refresh token portant `sub`, valide `ttl_seconds` à partir de `now`.
    pub fn issue_refresh(&self, sub: &str, ttl_seconds: i64, now: i64) -> Result<String> {
        self.issue(TokenKind::Refresh, sub, ttl_seconds, now)
    }

    /// Vérifie la signature, l'expiration (à `now`) et déserialize les claims.
    ///
    /// Les claims (`exp`, etc.) sont vérifiées par `jsonwebtoken` avec
    /// `now` injecté — pas d'appel à `SystemTime::now()` côté service,
    /// pour rester déterministe et testable.
    pub fn verify(&self, token: &str, now: i64) -> Result<Claims> {
        let mut validation = Validation::new(Algorithm::HS256);
        // L'horloge est injectée : on désactive la lecture system clock
        // de jsonwebtoken et on contrôle nous-mêmes `exp` après decode.
        validation.validate_exp = false;
        validation.required_spec_claims.clear();
        let data =
            decode::<Claims>(token, &self.decoding, &validation).map_err(|e| match e.kind() {
                ErrorKind::InvalidSignature | ErrorKind::Base64(_) | ErrorKind::Json(_) => {
                    Error::Invalid(format!("jwt invalide : {e}"))
                }
                other => Error::Invalid(format!("jwt erreur : {other:?}")),
            })?;
        let claims = data.claims;
        if claims.exp <= now {
            return Err(Error::Invalid("jwt expiré".into()));
        }
        Ok(claims)
    }

    /// Rotation : valide `refresh_token`, vérifie qu'il n'est pas révoqué,
    /// **le révoque** dans `store`, et émet une nouvelle paire
    /// `(access, refresh)`.
    ///
    /// Retourne `Err` si le token n'est pas un refresh, est expiré, est
    /// déjà révoqué ou n'est pas signé par ce service.
    pub async fn rotate<S: RevocationStore + ?Sized>(
        &self,
        refresh_token: &str,
        store: &S,
        access_ttl_seconds: i64,
        refresh_ttl_seconds: i64,
        now: i64,
    ) -> Result<(String, String)> {
        let claims = self.verify(refresh_token, now)?;
        if !matches!(claims.kind, TokenKind::Refresh) {
            return Err(Error::Invalid(
                "rotation : un access token n'est pas rotable".into(),
            ));
        }
        if store.is_revoked(&claims.jti).await? {
            return Err(Error::Invalid(
                "rotation : refresh token déjà révoqué".into(),
            ));
        }
        // exp en ms = exp (secondes) * 1000
        store
            .revoke(&claims.jti, claims.exp.saturating_mul(1_000))
            .await?;
        let access = self.issue_access(&claims.sub, access_ttl_seconds, now)?;
        let refresh = self.issue_refresh(&claims.sub, refresh_ttl_seconds, now)?;
        Ok((access, refresh))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::revocation::InMemoryRevocationStore;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const NOW: i64 = 1_700_000_000;

    fn svc() -> TokenService {
        TokenService::new(SECRET)
    }

    #[test]
    fn issue_then_verify_roundtrips_claims() {
        let s = svc();
        let token = s.issue_access("alice", 900, NOW).unwrap();
        let claims = s.verify(&token, NOW + 100).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.kind, TokenKind::Access);
        assert_eq!(claims.iat, NOW);
        assert_eq!(claims.exp, NOW + 900);
        assert_eq!(claims.jti.len(), 32);
    }

    #[test]
    fn refresh_token_has_refresh_kind() {
        let s = svc();
        let token = s.issue_refresh("bob", 86_400, NOW).unwrap();
        let claims = s.verify(&token, NOW + 100).unwrap();
        assert_eq!(claims.kind, TokenKind::Refresh);
    }

    #[test]
    fn expired_token_is_rejected() {
        let s = svc();
        let token = s.issue_access("alice", 60, NOW).unwrap();
        let err = s.verify(&token, NOW + 61).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("expiré")));
    }

    #[test]
    fn tampered_token_is_rejected() {
        let s = svc();
        let mut token = s.issue_access("alice", 900, NOW).unwrap();
        // Modifie un caractère dans la signature (dernière section)
        let last = token.pop().unwrap();
        let flipped = if last == 'A' { 'B' } else { 'A' };
        token.push(flipped);
        let err = s.verify(&token, NOW + 100).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("jwt")));
    }

    #[test]
    fn token_signed_with_different_secret_is_rejected() {
        let a = TokenService::new(b"secret-a");
        let b = TokenService::new(b"secret-b-different");
        let token = a.issue_access("u", 900, NOW).unwrap();
        let err = b.verify(&token, NOW + 100).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[test]
    fn jtis_differ_across_issuances() {
        let s = svc();
        let t1 = s.issue_access("u", 900, NOW).unwrap();
        let t2 = s.issue_access("u", 900, NOW).unwrap();
        let c1 = s.verify(&t1, NOW + 1).unwrap();
        let c2 = s.verify(&t2, NOW + 1).unwrap();
        assert_ne!(c1.jti, c2.jti);
    }

    #[tokio::test]
    async fn rotate_consumes_refresh_and_issues_new_pair() {
        let s = svc();
        let store = InMemoryRevocationStore::new();
        let refresh = s.issue_refresh("u", 3_600, NOW).unwrap();
        let original_jti = s.verify(&refresh, NOW + 1).unwrap().jti;

        let (access, new_refresh) = s
            .rotate(&refresh, &store, 900, 3_600, NOW + 10)
            .await
            .unwrap();
        // L'ancien refresh est désormais révoqué
        assert!(store.is_revoked(&original_jti).await.unwrap());
        // Le nouvel access est un Access valide
        let ac = s.verify(&access, NOW + 11).unwrap();
        assert_eq!(ac.kind, TokenKind::Access);
        assert_eq!(ac.sub, "u");
        // Le nouveau refresh est un Refresh valide et différent
        let rc = s.verify(&new_refresh, NOW + 11).unwrap();
        assert_eq!(rc.kind, TokenKind::Refresh);
        assert_ne!(rc.jti, original_jti);
    }

    #[tokio::test]
    async fn rotate_rejects_already_revoked_refresh() {
        let s = svc();
        let store = InMemoryRevocationStore::new();
        let refresh = s.issue_refresh("u", 3_600, NOW).unwrap();
        let _ = s
            .rotate(&refresh, &store, 900, 3_600, NOW + 10)
            .await
            .unwrap();
        // Deuxième rotation avec le même refresh → rejetée (replay attack)
        let err = s
            .rotate(&refresh, &store, 900, 3_600, NOW + 20)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("révoqué")));
    }

    #[tokio::test]
    async fn rotate_rejects_access_token() {
        let s = svc();
        let store = InMemoryRevocationStore::new();
        let access = s.issue_access("u", 900, NOW).unwrap();
        let err = s
            .rotate(&access, &store, 900, 3_600, NOW + 10)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("access")));
    }

    #[tokio::test]
    async fn rotate_rejects_expired_refresh() {
        let s = svc();
        let store = InMemoryRevocationStore::new();
        let refresh = s.issue_refresh("u", 60, NOW).unwrap();
        let err = s
            .rotate(&refresh, &store, 900, 3_600, NOW + 61)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("expiré")));
    }
}

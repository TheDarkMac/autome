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
//! [`crate::RevocationStore`] pour révoquer un token avant son `exp`, et la
//! version de session `sv` de leur utilisateur : incrémenter cette version
//! (« déconnecter toutes les sessions », #186) invalide d'un coup tous ses
//! jetons.
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
    /// Version de session de l'utilisateur à l'émission (claim custom, #186).
    /// Le jeton n'est valide que tant qu'elle égale la version courante
    /// ([`RevocationStore::session_version`]). Absente des jetons émis avant
    /// #186 ⇒ 0, la version initiale : ils restent valides.
    #[serde(default)]
    pub sv: i64,
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

    fn issue(
        &self,
        kind: TokenKind,
        sub: &str,
        sv: i64,
        ttl_seconds: i64,
        now: i64,
    ) -> Result<String> {
        let claims = Claims {
            sub: sub.to_string(),
            iat: now,
            exp: now + ttl_seconds,
            jti: fresh_jti(),
            kind,
            sv,
        };
        encode(&Self::header(), &claims, &self.encoding)
            .map_err(|e| Error::Invalid(format!("jwt encode : {e}")))
    }

    /// Émet un access token portant `sub`, valide `ttl_seconds` à partir de `now`.
    /// Version de session 0 (initiale) : pour une session réelle, préférer
    /// [`issue_pair`](Self::issue_pair) avec la version courante.
    pub fn issue_access(&self, sub: &str, ttl_seconds: i64, now: i64) -> Result<String> {
        self.issue(TokenKind::Access, sub, 0, ttl_seconds, now)
    }

    /// Émet un refresh token portant `sub`, valide `ttl_seconds` à partir de `now`.
    /// Version de session 0, comme [`issue_access`](Self::issue_access).
    pub fn issue_refresh(&self, sub: &str, ttl_seconds: i64, now: i64) -> Result<String> {
        self.issue(TokenKind::Refresh, sub, 0, ttl_seconds, now)
    }

    /// Émet une paire `(access, refresh)` pour `sub` à la version de session
    /// `sv` — celle lue dans le [`RevocationStore`] au moment de l'émission.
    pub fn issue_pair(
        &self,
        sub: &str,
        sv: i64,
        access_ttl_seconds: i64,
        refresh_ttl_seconds: i64,
        now: i64,
    ) -> Result<(String, String)> {
        Ok((
            self.issue(TokenKind::Access, sub, sv, access_ttl_seconds, now)?,
            self.issue(TokenKind::Refresh, sub, sv, refresh_ttl_seconds, now)?,
        ))
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
    /// déjà révoqué, appartient à une session fermée (version `sv` périmée,
    /// #186) ou n'est pas signé par ce service. La nouvelle paire porte la
    /// version de session courante.
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
        let current = store.session_version(&claims.sub).await?;
        if claims.sv != current {
            return Err(Error::Invalid(
                "rotation : session fermée (toutes les sessions déconnectées)".into(),
            ));
        }
        // Vérification et révocation en une seule opération atomique : de N
        // rotations concurrentes du même refresh, une seule passe (#184).
        // exp en ms = exp (secondes) * 1000
        if !store
            .revoke_once(&claims.jti, claims.exp.saturating_mul(1_000))
            .await?
        {
            return Err(Error::Invalid(
                "rotation : refresh token déjà révoqué".into(),
            ));
        }
        self.issue_pair(
            &claims.sub,
            current,
            access_ttl_seconds,
            refresh_ttl_seconds,
            now,
        )
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
    async fn concurrent_rotations_of_one_refresh_have_a_single_winner() {
        let s = std::sync::Arc::new(svc());
        let store = std::sync::Arc::new(InMemoryRevocationStore::new());
        let refresh = s.issue_refresh("u", 3_600, NOW).unwrap();
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let (s, store, refresh) = (s.clone(), store.clone(), refresh.clone());
                tokio::spawn(async move {
                    s.rotate(&refresh, store.as_ref(), 900, 3_600, NOW + 10)
                        .await
                        .is_ok()
                })
            })
            .collect();
        let mut winners = 0;
        for t in tasks {
            winners += usize::from(t.await.unwrap());
        }
        assert_eq!(winners, 1, "un refresh ne sert qu'une fois");
    }

    #[test]
    fn issue_pair_carries_session_version_and_kinds() {
        let s = svc();
        let (access, refresh) = s.issue_pair("u", 3, 900, 3_600, NOW).unwrap();
        let (ac, rc) = (
            s.verify(&access, NOW + 1).unwrap(),
            s.verify(&refresh, NOW + 1).unwrap(),
        );
        assert_eq!((ac.kind, ac.sv), (TokenKind::Access, 3));
        assert_eq!((rc.kind, rc.sv), (TokenKind::Refresh, 3));
        assert_eq!(rc.exp, NOW + 3_600);
    }

    #[test]
    fn token_without_sv_claim_decodes_as_version_zero() {
        #[derive(Serialize)]
        struct Legacy<'a> {
            sub: &'a str,
            iat: i64,
            exp: i64,
            jti: &'a str,
            kind: TokenKind,
        }
        let legacy = Legacy {
            sub: "u",
            iat: NOW,
            exp: NOW + 900,
            jti: "j",
            kind: TokenKind::Access,
        };
        let token = encode(
            &TokenService::header(),
            &legacy,
            &EncodingKey::from_secret(SECRET),
        )
        .unwrap();
        assert_eq!(svc().verify(&token, NOW + 1).unwrap().sv, 0);
    }

    #[tokio::test]
    async fn rotate_rejects_refresh_of_a_closed_session_and_issues_current_version() {
        let s = svc();
        let store = InMemoryRevocationStore::new();
        let old = s.issue_refresh("u", 3_600, NOW).unwrap();
        assert_eq!(store.bump_session_version("u").await.unwrap(), 1);
        let err = s
            .rotate(&old, &store, 900, 3_600, NOW + 10)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("session fermée")));

        // Une session ouverte après la déconnexion générale tourne normalement
        // et reste à la version courante.
        let (_, fresh) = s.issue_pair("u", 1, 900, 3_600, NOW + 20).unwrap();
        let (access, refresh) = s
            .rotate(&fresh, &store, 900, 3_600, NOW + 30)
            .await
            .unwrap();
        assert_eq!(s.verify(&access, NOW + 31).unwrap().sv, 1);
        assert_eq!(s.verify(&refresh, NOW + 31).unwrap().sv, 1);
        // Les autres utilisateurs ne sont pas touchés.
        let other = s.issue_refresh("v", 3_600, NOW).unwrap();
        assert!(s.rotate(&other, &store, 900, 3_600, NOW + 40).await.is_ok());
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

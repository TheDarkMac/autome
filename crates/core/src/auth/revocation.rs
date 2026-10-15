//! Révocation de JWT par `jti` (F6.1.d).
//!
//! Stratégie : on n'invalide pas un token côté serveur en mémoire absolu
//! (le JWT reste cryptographiquement valide jusqu'à son `exp`) ; on tient
//! une **liste de révocation** indexée par `jti`. Chaque entrée porte sa
//! propre `exp_ms` : on peut donc GC les entrées dont l'`exp` est dépassé,
//! sans risque d'accepter un token qui aurait été révoqué avant expiration.
//!
//! Seconde granularité (#186) : la **version de session** d'un utilisateur.
//! Chaque jeton porte la version courante à son émission (claim `sv`) ;
//! [`RevocationStore::bump_session_version`] l'incrémente et invalide ainsi
//! tous les jetons de l'utilisateur, sans les énumérer.
//!
//! L'impl in-process [`InMemoryRevocationStore`] suffit en mode single-node
//! et pour les tests ; `PgRevocationStore` (crate `autome-persistence`) la
//! partage entre répliques.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{Error, Result};

/// État de révocation d'un jeton, lu en un seul appel par l'extracteur
/// d'authentification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenState {
    /// Le `jti` est révoqué (logout, rotation).
    pub revoked: bool,
    /// Version de session courante de l'utilisateur du jeton.
    pub session_version: i64,
}

/// Liste de révocation de tokens (par `jti`) et versions de session (par
/// utilisateur).
///
/// **Async** (dette #107) : les impls réelles (PostgreSQL) font des I/O ; le
/// trait est donc asynchrone pour s'intégrer au runtime de l'API.
#[async_trait]
pub trait RevocationStore: Send + Sync {
    /// `true` si le token portant ce `jti` a été révoqué et que sa
    /// révocation n'est pas encore expirée.
    async fn is_revoked(&self, jti: &str) -> Result<bool>;

    /// Marque `jti` comme révoqué jusqu'à `exp_ms` (millisecondes Unix).
    /// Idempotent : ré-appeler ne change rien si le `exp_ms` est ≤.
    async fn revoke(&self, jti: &str, exp_ms: i64) -> Result<()>;

    /// Révoque `jti` jusqu'à `exp_ms` **seulement s'il ne l'était pas**, en une
    /// opération atomique. `true` : cet appel l'a révoqué ; `false` : il l'était
    /// déjà (entrée laissée telle quelle).
    ///
    /// Sert les jetons à usage unique (rotation du refresh, #184) : sur N appels
    /// concurrents — même répartis sur plusieurs répliques — un seul obtient
    /// `true`, là où `is_revoked` puis `revoke` laisseraient passer les N.
    async fn revoke_once(&self, jti: &str, exp_ms: i64) -> Result<bool>;

    /// Version de session courante de `user_id` (0 tant qu'aucune
    /// déconnexion générale n'a eu lieu).
    async fn session_version(&self, user_id: &str) -> Result<i64>;

    /// Incrémente atomiquement la version de session de `user_id` et renvoie
    /// la nouvelle : tous les jetons émis avant (version inférieure) cessent
    /// d'être acceptés. Des appels concurrents donnent des versions
    /// distinctes, aucun incrément n'est perdu.
    async fn bump_session_version(&self, user_id: &str) -> Result<i64>;

    /// [`is_revoked`](Self::is_revoked) + [`session_version`](Self::session_version)
    /// en un appel. Les impls distantes le surchargent pour n'effectuer
    /// qu'un aller-retour par requête authentifiée.
    async fn token_state(&self, jti: &str, user_id: &str) -> Result<TokenState> {
        Ok(TokenState {
            revoked: self.is_revoked(jti).await?,
            session_version: self.session_version(user_id).await?,
        })
    }

    /// Supprime les entrées dont `exp_ms` est `<= now_ms` (GC manuel).
    /// Retourne le nombre d'entrées supprimées.
    async fn gc(&self, now_ms: i64) -> Result<usize>;
}

/// Impl in-process, thread-safe (`Mutex<HashMap<String, i64>>`).
#[derive(Debug, Default)]
pub struct InMemoryRevocationStore {
    inner: Mutex<HashMap<String, i64>>,
    /// Version de session par utilisateur (absent ⇒ 0).
    sessions: Mutex<HashMap<String, i64>>,
}

impl InMemoryRevocationStore {
    /// Crée un store vide.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RevocationStore for InMemoryRevocationStore {
    async fn is_revoked(&self, jti: &str) -> Result<bool> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("revocation lock empoisonné".into()))?;
        Ok(map.contains_key(jti))
    }

    async fn revoke(&self, jti: &str, exp_ms: i64) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("revocation lock empoisonné".into()))?;
        map.entry(jti.to_string())
            .and_modify(|e| {
                if exp_ms > *e {
                    *e = exp_ms;
                }
            })
            .or_insert(exp_ms);
        Ok(())
    }

    async fn revoke_once(&self, jti: &str, exp_ms: i64) -> Result<bool> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("revocation lock empoisonné".into()))?;
        if map.contains_key(jti) {
            return Ok(false);
        }
        map.insert(jti.to_string(), exp_ms);
        Ok(true)
    }

    async fn session_version(&self, user_id: &str) -> Result<i64> {
        let map = self
            .sessions
            .lock()
            .map_err(|_| Error::Invalid("sessions lock empoisonné".into()))?;
        Ok(map.get(user_id).copied().unwrap_or(0))
    }

    async fn bump_session_version(&self, user_id: &str) -> Result<i64> {
        let mut map = self
            .sessions
            .lock()
            .map_err(|_| Error::Invalid("sessions lock empoisonné".into()))?;
        let version = map.entry(user_id.to_string()).or_insert(0);
        *version += 1;
        Ok(*version)
    }

    async fn gc(&self, now_ms: i64) -> Result<usize> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("revocation lock empoisonné".into()))?;
        let before = map.len();
        map.retain(|_jti, exp| *exp > now_ms);
        Ok(before - map.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_store_does_not_revoke() {
        let s = InMemoryRevocationStore::new();
        assert!(!s.is_revoked("any").await.unwrap());
    }

    #[tokio::test]
    async fn revoke_then_is_revoked_returns_true() {
        let s = InMemoryRevocationStore::new();
        s.revoke("jti-1", 1_000).await.unwrap();
        assert!(s.is_revoked("jti-1").await.unwrap());
        assert!(!s.is_revoked("jti-other").await.unwrap());
    }

    #[tokio::test]
    async fn revoke_is_idempotent_and_keeps_max_exp() {
        let s = InMemoryRevocationStore::new();
        s.revoke("j", 1_000).await.unwrap();
        s.revoke("j", 500).await.unwrap(); // exp plus petit → ignoré
        s.revoke("j", 2_000).await.unwrap(); // exp plus grand → étendu
        s.gc(1_500).await.unwrap();
        assert!(
            s.is_revoked("j").await.unwrap(),
            "exp=2000 doit survivre à gc(1500)"
        );
    }

    #[tokio::test]
    async fn revoke_once_succeeds_only_the_first_time() {
        let s = InMemoryRevocationStore::new();
        assert!(s.revoke_once("j", 1_000).await.unwrap());
        assert!(s.is_revoked("j").await.unwrap());
        assert!(!s.revoke_once("j", 2_000).await.unwrap());
        // Déjà révoqué par `revoke` : `revoke_once` ne réussit pas non plus.
        s.revoke("k", 1_000).await.unwrap();
        assert!(!s.revoke_once("k", 1_000).await.unwrap());
    }

    #[tokio::test]
    async fn revoke_once_leaves_existing_entry_untouched() {
        let s = InMemoryRevocationStore::new();
        s.revoke("j", 1_000).await.unwrap();
        assert!(!s.revoke_once("j", 9_000).await.unwrap());
        s.gc(1_000).await.unwrap();
        assert!(!s.is_revoked("j").await.unwrap());
    }

    #[tokio::test]
    async fn concurrent_revoke_once_has_a_single_winner() {
        let s = std::sync::Arc::new(InMemoryRevocationStore::new());
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move { s.revoke_once("j", 1_000).await.unwrap() })
            })
            .collect();
        let mut winners = 0;
        for t in tasks {
            winners += usize::from(t.await.unwrap());
        }
        assert_eq!(winners, 1);
    }

    #[tokio::test]
    async fn session_version_starts_at_zero_and_bumps_per_user() {
        let s = InMemoryRevocationStore::new();
        assert_eq!(s.session_version("u").await.unwrap(), 0);
        assert_eq!(s.bump_session_version("u").await.unwrap(), 1);
        assert_eq!(s.bump_session_version("u").await.unwrap(), 2);
        assert_eq!(s.session_version("u").await.unwrap(), 2);
        assert_eq!(s.session_version("v").await.unwrap(), 0);
        // Le GC des révocations ne touche pas aux versions de session.
        s.gc(i64::MAX).await.unwrap();
        assert_eq!(s.session_version("u").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn token_state_combines_jti_and_session_version() {
        let s = InMemoryRevocationStore::new();
        s.revoke("j", 1_000).await.unwrap();
        s.bump_session_version("u").await.unwrap();
        assert_eq!(
            s.token_state("j", "u").await.unwrap(),
            TokenState {
                revoked: true,
                session_version: 1
            }
        );
        assert_eq!(
            s.token_state("k", "v").await.unwrap(),
            TokenState {
                revoked: false,
                session_version: 0
            }
        );
    }

    #[tokio::test]
    async fn concurrent_bumps_are_not_lost() {
        let s = std::sync::Arc::new(InMemoryRevocationStore::new());
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move { s.bump_session_version("u").await.unwrap() })
            })
            .collect();
        let mut versions = Vec::new();
        for t in tasks {
            versions.push(t.await.unwrap());
        }
        versions.sort_unstable();
        assert_eq!(versions, (1..=16).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn gc_removes_expired_entries() {
        let s = InMemoryRevocationStore::new();
        s.revoke("old", 100).await.unwrap();
        s.revoke("future", 9_000).await.unwrap();
        let removed = s.gc(5_000).await.unwrap();
        assert_eq!(removed, 1);
        assert!(!s.is_revoked("old").await.unwrap());
        assert!(s.is_revoked("future").await.unwrap());
    }

    #[tokio::test]
    async fn gc_on_empty_store_returns_zero() {
        let s = InMemoryRevocationStore::new();
        assert_eq!(s.gc(123).await.unwrap(), 0);
    }

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryRevocationStore>();
    }
}

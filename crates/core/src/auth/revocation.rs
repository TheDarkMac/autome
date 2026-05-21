//! Révocation de JWT par `jti` (F6.1.d).
//!
//! Stratégie : on n'invalide pas un token côté serveur en mémoire absolu
//! (le JWT reste cryptographiquement valide jusqu'à son `exp`) ; on tient
//! une **liste de révocation** indexée par `jti`. Chaque entrée porte sa
//! propre `exp_ms` : on peut donc GC les entrées dont l'`exp` est dépassé,
//! sans risque d'accepter un token qui aurait été révoqué avant expiration.
//!
//! L'impl in-process [`InMemoryRevocationStore`] suffit en mode single-node
//! et pour les tests. Une impl Redis viendra plus tard (multi-replica).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{Error, Result};

/// Liste de révocation de tokens (par `jti`).
pub trait RevocationStore {
    /// `true` si le token portant ce `jti` a été révoqué et que sa
    /// révocation n'est pas encore expirée.
    fn is_revoked(&self, jti: &str) -> Result<bool>;

    /// Marque `jti` comme révoqué jusqu'à `exp_ms` (millisecondes Unix).
    /// Idempotent : ré-appeler ne change rien si le `exp_ms` est ≤.
    fn revoke(&self, jti: &str, exp_ms: i64) -> Result<()>;

    /// Supprime les entrées dont `exp_ms` est `<= now_ms` (GC manuel).
    /// Retourne le nombre d'entrées supprimées.
    fn gc(&self, now_ms: i64) -> Result<usize>;
}

/// Impl in-process, thread-safe (`Mutex<HashMap<String, i64>>`).
#[derive(Debug, Default)]
pub struct InMemoryRevocationStore {
    inner: Mutex<HashMap<String, i64>>,
}

impl InMemoryRevocationStore {
    /// Crée un store vide.
    pub fn new() -> Self {
        Self::default()
    }
}

impl RevocationStore for InMemoryRevocationStore {
    fn is_revoked(&self, jti: &str) -> Result<bool> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("revocation lock empoisonné".into()))?;
        Ok(map.contains_key(jti))
    }

    fn revoke(&self, jti: &str, exp_ms: i64) -> Result<()> {
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

    fn gc(&self, now_ms: i64) -> Result<usize> {
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

    #[test]
    fn fresh_store_does_not_revoke() {
        let s = InMemoryRevocationStore::new();
        assert!(!s.is_revoked("any").unwrap());
    }

    #[test]
    fn revoke_then_is_revoked_returns_true() {
        let s = InMemoryRevocationStore::new();
        s.revoke("jti-1", 1_000).unwrap();
        assert!(s.is_revoked("jti-1").unwrap());
        assert!(!s.is_revoked("jti-other").unwrap());
    }

    #[test]
    fn revoke_is_idempotent_and_keeps_max_exp() {
        let s = InMemoryRevocationStore::new();
        s.revoke("j", 1_000).unwrap();
        s.revoke("j", 500).unwrap(); // exp plus petit → ignoré
        s.revoke("j", 2_000).unwrap(); // exp plus grand → étendu
        s.gc(1_500).unwrap();
        assert!(
            s.is_revoked("j").unwrap(),
            "exp=2000 doit survivre à gc(1500)"
        );
    }

    #[test]
    fn gc_removes_expired_entries() {
        let s = InMemoryRevocationStore::new();
        s.revoke("old", 100).unwrap();
        s.revoke("future", 9_000).unwrap();
        let removed = s.gc(5_000).unwrap();
        assert_eq!(removed, 1);
        assert!(!s.is_revoked("old").unwrap());
        assert!(s.is_revoked("future").unwrap());
    }

    #[test]
    fn gc_on_empty_store_returns_zero() {
        let s = InMemoryRevocationStore::new();
        assert_eq!(s.gc(123).unwrap(), 0);
    }

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryRevocationStore>();
    }
}

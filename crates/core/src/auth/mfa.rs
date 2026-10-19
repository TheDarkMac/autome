//! Second facteur TOTP : persistance (#245).
//!
//! Le contrat [`MfaStore`] ne connaît que des valeurs **opaques** : le secret
//! TOTP déjà chiffré (enveloppe AES-GCM, cf. [`crate::EnvelopeCipher`]) et les
//! empreintes des codes de secours (HMAC, jamais le code). Le calcul des codes
//! et le chiffrement vivent dans l'API.
//!
//! Cycle de vie d'un compte :
//!
//! 1. [`begin_enrollment`](MfaStore::begin_enrollment) pose un secret **en
//!    attente** (remplace un éventuel précédent, jamais un secret actif) ;
//! 2. [`enable`](MfaStore::enable) l'active si le secret en attente est
//!    toujours celui dont l'utilisateur vient de saisir un code (écriture
//!    conditionnelle), et remplace les codes de secours ;
//! 3. à chaque code TOTP accepté, [`consume_step`](MfaStore::consume_step)
//!    enregistre son pas de temps **seulement s'il est postérieur** au
//!    dernier : un code intercepté ne sert pas deux fois, y compris entre
//!    requêtes concurrentes ;
//! 4. [`consume_recovery_code`](MfaStore::consume_recovery_code) brûle un code
//!    de secours (usage unique, atomique) ;
//! 5. [`disable`](MfaStore::disable) efface tout (désactivation par
//!    l'utilisateur, réinitialisation par un administrateur).
//!
//! En PostgreSQL, les lignes disparaissent avec le compte (clé étrangère en
//! cascade) : l'effacement RGPD (#229) n'a rien à faire de plus.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{Error, Result};

/// État MFA d'un compte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MfaRecord {
    /// Secret TOTP chiffré (enveloppe opaque).
    pub secret: String,
    /// `true` une fois l'enrôlement confirmé par un premier code ; `false`
    /// tant qu'il est en attente.
    pub enabled: bool,
    /// Dernier pas de temps TOTP accepté (anti-rejeu) ; `None` avant le
    /// premier.
    pub last_step: Option<i64>,
    /// Codes de secours encore utilisables.
    pub recovery_codes_left: usize,
}

/// Persistance du second facteur.
#[async_trait]
pub trait MfaStore: Send + Sync {
    /// État MFA de `user_id` ; `None` si aucun enrôlement (ni en attente ni
    /// actif), y compris pour un identifiant inconnu ou mal formé.
    async fn get(&self, user_id: &str) -> Result<Option<MfaRecord>>;

    /// Pose `secret` en attente de confirmation, à la place d'un éventuel
    /// secret en attente. `false` si la MFA est déjà active (rien n'est
    /// écrit) ou si le compte est inconnu.
    async fn begin_enrollment(&self, user_id: &str, secret: &str) -> Result<bool>;

    /// Active la MFA **seulement** si le secret en attente vaut encore
    /// `secret` : pas `step` consommé, codes de secours remplacés par
    /// `recovery_hashes`, atomiquement. `false` sinon (déjà active, autre
    /// secret posé entre-temps, aucun enrôlement).
    async fn enable(
        &self,
        user_id: &str,
        secret: &str,
        step: i64,
        recovery_hashes: &[String],
    ) -> Result<bool>;

    /// Consomme le pas TOTP `step` d'une MFA active, s'il est strictement
    /// postérieur au dernier consommé. `false` sinon (rejeu, MFA inactive).
    async fn consume_step(&self, user_id: &str, step: i64) -> Result<bool>;

    /// Brûle le code de secours d'empreinte `code_hash` d'une MFA active.
    /// `false` s'il n'existe pas ou a déjà servi.
    async fn consume_recovery_code(&self, user_id: &str, code_hash: &str) -> Result<bool>;

    /// Remplace tous les codes de secours d'une MFA active. `false` si la MFA
    /// n'est pas active.
    async fn replace_recovery_codes(
        &self,
        user_id: &str,
        recovery_hashes: &[String],
    ) -> Result<bool>;

    /// Supprime l'enrôlement (actif ou en attente) et les codes de secours.
    /// `false` s'il n'y avait rien.
    async fn disable(&self, user_id: &str) -> Result<bool>;

    /// Parmi `user_ids`, ceux dont la MFA est active, en une lecture (liste
    /// d'administration).
    async fn enabled_among(&self, user_ids: &[String]) -> Result<HashSet<String>>;
}

#[derive(Debug, Clone)]
struct Entry {
    secret: String,
    enabled: bool,
    last_step: Option<i64>,
    recovery: HashSet<String>,
}

/// [`MfaStore`] en mémoire (dev, tests, mono-instance). Il ne vérifie pas
/// l'existence du compte : l'API ne l'appelle que pour un compte qu'elle
/// vient de lire.
#[derive(Debug, Default)]
pub struct InMemoryMfaStore {
    entries: Mutex<HashMap<String, Entry>>,
}

impl InMemoryMfaStore {
    /// Store vide.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Entry>>> {
        self.entries
            .lock()
            .map_err(|_| Error::Invalid("mfa store poisoned".into()))
    }
}

#[async_trait]
impl MfaStore for InMemoryMfaStore {
    async fn get(&self, user_id: &str) -> Result<Option<MfaRecord>> {
        Ok(self.lock()?.get(user_id).map(|e| MfaRecord {
            secret: e.secret.clone(),
            enabled: e.enabled,
            last_step: e.last_step,
            recovery_codes_left: e.recovery.len(),
        }))
    }

    async fn begin_enrollment(&self, user_id: &str, secret: &str) -> Result<bool> {
        let mut entries = self.lock()?;
        if entries.get(user_id).is_some_and(|e| e.enabled) {
            return Ok(false);
        }
        entries.insert(
            user_id.to_string(),
            Entry {
                secret: secret.to_string(),
                enabled: false,
                last_step: None,
                recovery: HashSet::new(),
            },
        );
        Ok(true)
    }

    async fn enable(
        &self,
        user_id: &str,
        secret: &str,
        step: i64,
        recovery_hashes: &[String],
    ) -> Result<bool> {
        let mut entries = self.lock()?;
        match entries.get_mut(user_id) {
            Some(e) if !e.enabled && e.secret == secret => {
                e.enabled = true;
                e.last_step = Some(step);
                e.recovery = recovery_hashes.iter().cloned().collect();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn consume_step(&self, user_id: &str, step: i64) -> Result<bool> {
        let mut entries = self.lock()?;
        match entries.get_mut(user_id) {
            Some(e) if e.enabled && e.last_step.is_none_or(|last| step > last) => {
                e.last_step = Some(step);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn consume_recovery_code(&self, user_id: &str, code_hash: &str) -> Result<bool> {
        let mut entries = self.lock()?;
        Ok(entries
            .get_mut(user_id)
            .is_some_and(|e| e.enabled && e.recovery.remove(code_hash)))
    }

    async fn replace_recovery_codes(
        &self,
        user_id: &str,
        recovery_hashes: &[String],
    ) -> Result<bool> {
        let mut entries = self.lock()?;
        match entries.get_mut(user_id) {
            Some(e) if e.enabled => {
                e.recovery = recovery_hashes.iter().cloned().collect();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn disable(&self, user_id: &str) -> Result<bool> {
        Ok(self.lock()?.remove(user_id).is_some())
    }

    async fn enabled_among(&self, user_ids: &[String]) -> Result<HashSet<String>> {
        let entries = self.lock()?;
        Ok(user_ids
            .iter()
            .filter(|id| entries.get(id.as_str()).is_some_and(|e| e.enabled))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|c| (*c).to_string()).collect()
    }

    #[tokio::test]
    async fn enrollment_is_pending_until_confirmed_with_the_same_secret() {
        let store = InMemoryMfaStore::new();
        assert_eq!(store.get("u1").await.unwrap(), None);
        assert!(store.begin_enrollment("u1", "s1").await.unwrap());
        // Un nouvel enrôlement remplace le secret en attente.
        assert!(store.begin_enrollment("u1", "s2").await.unwrap());
        let pending = store.get("u1").await.unwrap().unwrap();
        assert_eq!((pending.secret.as_str(), pending.enabled), ("s2", false));
        // Code saisi pour l'ancien secret : refusé.
        assert!(!store.enable("u1", "s1", 10, &[]).await.unwrap());
        assert!(store
            .enable("u1", "s2", 10, &hashes(&["h1", "h2"]))
            .await
            .unwrap());
        let active = store.get("u1").await.unwrap().unwrap();
        assert_eq!(
            active,
            MfaRecord {
                secret: "s2".into(),
                enabled: true,
                last_step: Some(10),
                recovery_codes_left: 2,
            }
        );
        // Active : ni nouvel enrôlement, ni seconde activation.
        assert!(!store.begin_enrollment("u1", "s3").await.unwrap());
        assert!(!store.enable("u1", "s2", 11, &[]).await.unwrap());
        assert_eq!(
            store.enabled_among(&hashes(&["u1", "u2"])).await.unwrap(),
            HashSet::from(["u1".to_string()])
        );
    }

    #[tokio::test]
    async fn steps_are_consumed_once_and_in_order() {
        let store = InMemoryMfaStore::new();
        store.begin_enrollment("u1", "s").await.unwrap();
        // Pas d'anti-rejeu tant que la MFA n'est pas active.
        assert!(!store.consume_step("u1", 5).await.unwrap());
        store.enable("u1", "s", 10, &[]).await.unwrap();
        assert!(!store.consume_step("u1", 10).await.unwrap(), "rejeu");
        assert!(!store.consume_step("u1", 9).await.unwrap(), "pas antérieur");
        assert!(store.consume_step("u1", 11).await.unwrap());
        assert!(!store.consume_step("u1", 11).await.unwrap());
        assert!(!store.consume_step("unknown", 12).await.unwrap());
    }

    #[tokio::test]
    async fn recovery_codes_are_single_use_and_replaceable() {
        let store = InMemoryMfaStore::new();
        store.begin_enrollment("u1", "s").await.unwrap();
        assert!(!store
            .replace_recovery_codes("u1", &hashes(&["x"]))
            .await
            .unwrap());
        store
            .enable("u1", "s", 1, &hashes(&["h1", "h2"]))
            .await
            .unwrap();
        assert!(store.consume_recovery_code("u1", "h1").await.unwrap());
        assert!(!store.consume_recovery_code("u1", "h1").await.unwrap());
        assert_eq!(
            store.get("u1").await.unwrap().unwrap().recovery_codes_left,
            1
        );
        assert!(store
            .replace_recovery_codes("u1", &hashes(&["h3"]))
            .await
            .unwrap());
        assert!(!store.consume_recovery_code("u1", "h2").await.unwrap());
        assert!(store.consume_recovery_code("u1", "h3").await.unwrap());
    }

    #[tokio::test]
    async fn disable_removes_everything() {
        let store = InMemoryMfaStore::new();
        assert!(!store.disable("u1").await.unwrap());
        store.begin_enrollment("u1", "s").await.unwrap();
        store.enable("u1", "s", 1, &hashes(&["h"])).await.unwrap();
        assert!(store.disable("u1").await.unwrap());
        assert_eq!(store.get("u1").await.unwrap(), None);
        assert!(!store.consume_recovery_code("u1", "h").await.unwrap());
        // Réenrôlement possible ensuite.
        assert!(store.begin_enrollment("u1", "s2").await.unwrap());
    }
}

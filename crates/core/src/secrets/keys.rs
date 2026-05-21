//! Fournisseurs de clés AES-256 (F6.3.b).
//!
//! Le trait [`KeyProvider`] expose les opérations dont a besoin
//! [`crate::secrets::EnvelopeCipher`] :
//!
//! - `current()` : `key_id` à utiliser pour les **nouveaux** chiffrements ;
//! - `get(key_id)` : récupération d'**une** clé connue (les anciennes
//!   restent accessibles pour `decrypt`, sinon la rotation casse les
//!   enregistrements en place).
//!
//! L'impl `InMemoryKeyProvider` couvre les tests et le mode dev. En prod,
//! une impl `KmsKeyProvider` (AWS KMS, GCP KMS, Vault Transit) sera
//! branchée — le contrat suffit.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{Error, Result};

/// Longueur attendue d'une clé AES-256.
pub const KEY_LEN: usize = 32;

/// Fournit les clés AES-256 indexées par `key_id`.
pub trait KeyProvider {
    /// Renvoie la clé associée à `key_id`, ou `Err` si inconnue.
    fn get(&self, key_id: &str) -> Result<[u8; KEY_LEN]>;

    /// Renvoie l'identifiant de la clé **active** (utilisée pour chiffrer
    /// les nouveaux secrets).
    fn current(&self) -> Result<String>;
}

/// Impl en mémoire : `HashMap<key_id, key>` sous `Mutex`.
///
/// La clé courante peut être changée via [`InMemoryKeyProvider::set_current`] ;
/// les anciennes restent accessibles tant qu'elles ne sont pas explicitement
/// retirées (`remove_key`).
#[derive(Debug, Default)]
pub struct InMemoryKeyProvider {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    keys: HashMap<String, [u8; KEY_LEN]>,
    current: Option<String>,
}

impl InMemoryKeyProvider {
    /// Crée un fournisseur vide. **`current()`** échouera tant qu'aucune
    /// clé n'a été ajoutée.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construit un fournisseur préchargé avec une unique clé, qui devient
    /// par construction la clé courante.
    pub fn with_key(id: impl Into<String>, key: [u8; KEY_LEN]) -> Self {
        let store = Self::new();
        store
            .add_key(id, key, true)
            .expect("ajout initial ne peut pas échouer");
        store
    }

    /// Ajoute une clé.
    ///
    /// Si `make_current == true`, elle devient la clé active (les
    /// chiffrements suivants l'utilisent ; les anciens enregistrements
    /// restent déchiffrables tant que leur clé est toujours présente).
    ///
    /// `Err` si `id` est déjà connu (sécurité : on ne réutilise jamais
    /// un `key_id` pour une autre clé).
    pub fn add_key(
        &self,
        id: impl Into<String>,
        key: [u8; KEY_LEN],
        make_current: bool,
    ) -> Result<()> {
        let id = id.into();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("keystore lock empoisonné".into()))?;
        if inner.keys.contains_key(&id) {
            return Err(Error::Invalid(format!("key_id '{id}' déjà enregistré")));
        }
        inner.keys.insert(id.clone(), key);
        if make_current || inner.current.is_none() {
            inner.current = Some(id);
        }
        Ok(())
    }

    /// Définit la clé courante par son `id`. `Err` si l'id est inconnu.
    pub fn set_current(&self, id: &str) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("keystore lock empoisonné".into()))?;
        if !inner.keys.contains_key(id) {
            return Err(Error::Invalid(format!(
                "set_current : key_id '{id}' inconnu"
            )));
        }
        inner.current = Some(id.to_string());
        Ok(())
    }

    /// Retire une clé. **Attention** : tout secret chiffré avec cette clé
    /// devient illisible. À n'utiliser qu'après re-chiffrement complet.
    pub fn remove_key(&self, id: &str) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("keystore lock empoisonné".into()))?;
        if inner.current.as_deref() == Some(id) {
            return Err(Error::Invalid(format!(
                "remove_key : '{id}' est la clé courante"
            )));
        }
        inner
            .keys
            .remove(id)
            .ok_or_else(|| Error::Invalid(format!("remove_key : '{id}' inconnu")))?;
        Ok(())
    }
}

impl KeyProvider for InMemoryKeyProvider {
    fn get(&self, key_id: &str) -> Result<[u8; KEY_LEN]> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("keystore lock empoisonné".into()))?;
        inner
            .keys
            .get(key_id)
            .copied()
            .ok_or_else(|| Error::Invalid(format!("key_id '{key_id}' inconnu")))
    }

    fn current(&self) -> Result<String> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("keystore lock empoisonné".into()))?;
        inner
            .current
            .clone()
            .ok_or_else(|| Error::Invalid("keystore : aucune clé courante".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(b: u8) -> [u8; KEY_LEN] {
        [b; KEY_LEN]
    }

    #[test]
    fn empty_store_has_no_current() {
        let s = InMemoryKeyProvider::new();
        assert!(s.current().is_err());
    }

    #[test]
    fn first_added_key_becomes_current() {
        let s = InMemoryKeyProvider::new();
        s.add_key("k1", k(1), false).unwrap();
        // Même make_current=false, la première clé devient courante par défaut.
        assert_eq!(s.current().unwrap(), "k1");
    }

    #[test]
    fn add_key_with_make_current_overrides() {
        let s = InMemoryKeyProvider::with_key("k1", k(1));
        s.add_key("k2", k(2), true).unwrap();
        assert_eq!(s.current().unwrap(), "k2");
        // Mais k1 reste accessible
        assert_eq!(s.get("k1").unwrap(), k(1));
    }

    #[test]
    fn add_key_without_make_current_leaves_current_alone() {
        let s = InMemoryKeyProvider::with_key("k1", k(1));
        s.add_key("k2", k(2), false).unwrap();
        assert_eq!(s.current().unwrap(), "k1");
        assert_eq!(s.get("k2").unwrap(), k(2));
    }

    #[test]
    fn duplicate_key_id_is_rejected() {
        let s = InMemoryKeyProvider::with_key("k1", k(1));
        let err = s.add_key("k1", k(2), true).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("déjà")));
    }

    #[test]
    fn set_current_unknown_id_errors() {
        let s = InMemoryKeyProvider::with_key("k1", k(1));
        let err = s.set_current("nope").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    #[test]
    fn remove_current_key_is_refused() {
        let s = InMemoryKeyProvider::with_key("k1", k(1));
        let err = s.remove_key("k1").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("courante")));
    }

    #[test]
    fn remove_old_key_succeeds() {
        let s = InMemoryKeyProvider::with_key("k1", k(1));
        s.add_key("k2", k(2), true).unwrap();
        s.remove_key("k1").unwrap();
        assert!(s.get("k1").is_err());
        assert_eq!(s.get("k2").unwrap(), k(2));
    }

    #[test]
    fn get_unknown_key_errors() {
        let s = InMemoryKeyProvider::with_key("k1", k(1));
        let err = s.get("nope").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryKeyProvider>();
    }
}

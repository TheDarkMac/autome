//! Stockage chiffré des tokens connecteurs (F6.2.b).
//!
//! [`ConnectorTokenStore`] est indexé par `(user_id, provider_id)` et conserve
//! les [`TokenSet`] **chiffrés via `EnvelopeCipher`** (F6.3). Le sérialise est
//! du JSON (compact, debuggable).
//!
//! L'impl [`InMemoryConnectorTokenStore`] suffit pour tests et mode dev ;
//! une impl PG suivra avec sqlx.

use std::collections::HashMap;
use std::sync::Mutex;

use autome_core::{EnvelopeCipher, Error, KeyProvider, Result};
use serde::{Deserialize, Serialize};

use crate::oauth::client::TokenSet;

/// Forme sérialisable d'un [`TokenSet`] (pour stockage / transport).
///
/// On évite de mettre serde directement sur `TokenSet` pour garder la
/// crate d'origine clean ; la conversion est triviale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SerializableTokenSet {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: Option<i64>,
    token_type: String,
    scope: Option<String>,
}

impl From<&TokenSet> for SerializableTokenSet {
    fn from(t: &TokenSet) -> Self {
        Self {
            access_token: t.access_token.clone(),
            refresh_token: t.refresh_token.clone(),
            expires_at_ms: t.expires_at_ms,
            token_type: t.token_type.clone(),
            scope: t.scope.clone(),
        }
    }
}

impl From<SerializableTokenSet> for TokenSet {
    fn from(s: SerializableTokenSet) -> Self {
        Self {
            access_token: s.access_token,
            refresh_token: s.refresh_token,
            expires_at_ms: s.expires_at_ms,
            token_type: s.token_type,
            scope: s.scope,
        }
    }
}

/// Contrat de stockage des tokens connecteurs.
///
/// Le contenu est **toujours chiffré au repos** — l'impl doit utiliser
/// `EnvelopeCipher` (F6.3) ou équivalent côté backend.
pub trait ConnectorTokenStore {
    /// Persiste `tokens` pour `(user_id, provider_id)`. Écrase l'ancien.
    fn store(&self, user_id: &str, provider_id: &str, tokens: &TokenSet) -> Result<()>;

    /// Charge les tokens si présents.
    fn load(&self, user_id: &str, provider_id: &str) -> Result<Option<TokenSet>>;

    /// Supprime les tokens. No-op si absents.
    fn delete(&self, user_id: &str, provider_id: &str) -> Result<()>;
}

/// Impl in-process : HashMap sous Mutex, valeurs chiffrées via `EnvelopeCipher`.
pub struct InMemoryConnectorTokenStore<K: KeyProvider> {
    cipher: EnvelopeCipher<K>,
    inner: Mutex<HashMap<(String, String), String>>,
}

impl<K: KeyProvider> InMemoryConnectorTokenStore<K> {
    /// Crée un store qui utilise `cipher` pour chiffrer les tokens.
    pub fn new(cipher: EnvelopeCipher<K>) -> Self {
        Self {
            cipher,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Accès au cipher (utile pour les tests / rotation de clé).
    pub fn cipher(&self) -> &EnvelopeCipher<K> {
        &self.cipher
    }

    /// Nombre d'entrées (avant chiffrement, donc compte tous les `(user, provider)`).
    pub fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// `true` si aucun token stocké.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K: KeyProvider> ConnectorTokenStore for InMemoryConnectorTokenStore<K> {
    fn store(&self, user_id: &str, provider_id: &str, tokens: &TokenSet) -> Result<()> {
        let serializable: SerializableTokenSet = tokens.into();
        let json = serde_json::to_string(&serializable)?;
        let envelope = self.cipher.encrypt(json.as_bytes())?;
        self.inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?
            .insert((user_id.to_string(), provider_id.to_string()), envelope);
        Ok(())
    }

    fn load(&self, user_id: &str, provider_id: &str) -> Result<Option<TokenSet>> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?;
        let Some(envelope) = map.get(&(user_id.to_string(), provider_id.to_string())) else {
            return Ok(None);
        };
        let plaintext = self.cipher.decrypt(envelope)?;
        let serializable: SerializableTokenSet = serde_json::from_slice(&plaintext)?;
        Ok(Some(serializable.into()))
    }

    fn delete(&self, user_id: &str, provider_id: &str) -> Result<()> {
        self.inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?
            .remove(&(user_id.to_string(), provider_id.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::InMemoryKeyProvider;

    fn make_store() -> InMemoryConnectorTokenStore<InMemoryKeyProvider> {
        let provider = InMemoryKeyProvider::with_key("k1", [1u8; 32]);
        let cipher = EnvelopeCipher::new(provider);
        InMemoryConnectorTokenStore::new(cipher)
    }

    fn sample_tokens() -> TokenSet {
        TokenSet {
            access_token: "at-1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at_ms: Some(1_700_000_000_000),
            token_type: "Bearer".into(),
            scope: Some("openid email".into()),
        }
    }

    #[test]
    fn store_then_load_round_trips() {
        let store = make_store();
        store.store("alice", "google", &sample_tokens()).unwrap();
        let loaded = store.load("alice", "google").unwrap().unwrap();
        assert_eq!(loaded, sample_tokens());
    }

    #[test]
    fn load_missing_entry_returns_none() {
        let store = make_store();
        assert!(store.load("alice", "google").unwrap().is_none());
    }

    #[test]
    fn store_overwrites_existing() {
        let store = make_store();
        store.store("alice", "google", &sample_tokens()).unwrap();
        let new = TokenSet {
            access_token: "at-2".into(),
            refresh_token: Some("rt-2".into()),
            expires_at_ms: Some(2_000_000_000_000),
            token_type: "Bearer".into(),
            scope: None,
        };
        store.store("alice", "google", &new).unwrap();
        let loaded = store.load("alice", "google").unwrap().unwrap();
        assert_eq!(loaded, new);
    }

    #[test]
    fn delete_removes_entry() {
        let store = make_store();
        store.store("alice", "google", &sample_tokens()).unwrap();
        assert!(!store.is_empty());
        store.delete("alice", "google").unwrap();
        assert!(store.is_empty());
        assert!(store.load("alice", "google").unwrap().is_none());
    }

    #[test]
    fn delete_unknown_entry_is_noop() {
        let store = make_store();
        store.delete("nobody", "noplace").unwrap();
    }

    #[test]
    fn entries_are_isolated_by_user_and_provider() {
        let store = make_store();
        let alice_google = TokenSet {
            access_token: "alice@google".into(),
            ..sample_tokens()
        };
        let alice_slack = TokenSet {
            access_token: "alice@slack".into(),
            ..sample_tokens()
        };
        let bob_google = TokenSet {
            access_token: "bob@google".into(),
            ..sample_tokens()
        };
        store.store("alice", "google", &alice_google).unwrap();
        store.store("alice", "slack", &alice_slack).unwrap();
        store.store("bob", "google", &bob_google).unwrap();

        assert_eq!(
            store.load("alice", "google").unwrap().unwrap().access_token,
            "alice@google"
        );
        assert_eq!(
            store.load("alice", "slack").unwrap().unwrap().access_token,
            "alice@slack"
        );
        assert_eq!(
            store.load("bob", "google").unwrap().unwrap().access_token,
            "bob@google"
        );
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn stored_envelope_is_not_plaintext_at_rest() {
        let store = make_store();
        store.store("alice", "google", &sample_tokens()).unwrap();
        // Inspect le map interne — l'envelope ne contient pas `access_token` en clair.
        let inner = store.inner.lock().unwrap();
        let envelope = inner.values().next().unwrap();
        assert!(envelope.starts_with("v1:k1:"), "format envelope F6.3");
        assert!(
            !envelope.contains("at-1"),
            "le token en clair NE DOIT PAS apparaître"
        );
    }
}

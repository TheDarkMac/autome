//! Stockage chiffré des tokens connecteurs (F6.2.b).
//!
//! [`ConnectorTokenStore`] est indexé par `(user_id, provider_id)` et conserve
//! les [`TokenSet`] **chiffrés via `EnvelopeCipher`** (F6.3). Le sérialise est
//! du JSON (compact, debuggable).
//!
//! L'impl [`InMemoryConnectorTokenStore`] suffit pour tests et mode dev ;
//! l'impl PostgreSQL (`autome_persistence::PgConnectorTokenStore`) réutilise
//! [`seal_tokens`] / [`open_tokens`] : même format d'enveloppe au repos.
//!
//! Le rafraîchissement passe par [`ConnectorTokenStore::update_locked`] :
//! lecture et réécriture sous **verrou exclusif** par `(user_id, provider_id)`
//! — entre processus pour PostgreSQL — afin qu'un refresh token ne soit
//! présenté qu'une fois au provider, même avec plusieurs répliques du worker.

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

/// Chiffre `tokens` en enveloppe `EnvelopeCipher` (JSON sérialisé puis
/// chiffré) — forme stockée au repos par tous les [`ConnectorTokenStore`].
pub fn seal_tokens<K: KeyProvider>(
    cipher: &EnvelopeCipher<K>,
    tokens: &TokenSet,
) -> Result<String> {
    let serializable: SerializableTokenSet = tokens.into();
    let json = serde_json::to_string(&serializable)?;
    cipher.encrypt(json.as_bytes())
}

/// Déchiffre une enveloppe produite par [`seal_tokens`].
pub fn open_tokens<K: KeyProvider>(cipher: &EnvelopeCipher<K>, envelope: &str) -> Result<TokenSet> {
    let plaintext = cipher.decrypt(envelope)?;
    let serializable: SerializableTokenSet = serde_json::from_slice(&plaintext)?;
    Ok(serializable.into())
}

/// Marque posée sur une credential dont le provider a refusé le refresh token
/// (`invalid_grant`) : inutile de le lui représenter avant une reconnexion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInvalidation {
    /// Horodatage du refus (Unix ms).
    pub at_ms: i64,
    /// Motif lisible (réponse du provider), sans secret.
    pub reason: String,
}

/// Credential telle que stockée : tokens + marque d'invalidation éventuelle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCredential {
    /// Derniers tokens obtenus du provider.
    pub tokens: TokenSet,
    /// `Some` si le provider a refusé le refresh token depuis.
    pub invalidated: Option<CredentialInvalidation>,
}

/// Décision prise sous le verrou de [`ConnectorTokenStore::update_locked`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialUpdate {
    /// Rien à écrire.
    Keep,
    /// Remplace les tokens (rafraîchis) et efface toute marque d'invalidation.
    Replace(TokenSet),
    /// Garde les tokens et pose la marque d'invalidation.
    Invalidate(CredentialInvalidation),
}

/// Contrat de stockage des tokens connecteurs.
///
/// Le contenu est **toujours chiffré au repos** — l'impl doit utiliser
/// `EnvelopeCipher` (F6.3) ou équivalent côté backend.
pub trait ConnectorTokenStore {
    /// Persiste `tokens` pour `(user_id, provider_id)`. Écrase l'ancien et
    /// efface toute marque d'invalidation (reconnexion du compte).
    fn store(&self, user_id: &str, provider_id: &str, tokens: &TokenSet) -> Result<()>;

    /// Charge les tokens si présents (même marqués invalides).
    fn load(&self, user_id: &str, provider_id: &str) -> Result<Option<TokenSet>>;

    /// Lit la credential sous **verrou exclusif** sur `(user_id, provider_id)`
    /// puis applique la décision de `update`, avant de relâcher le verrou.
    ///
    /// Deux appels concurrents pour la même clé — y compris depuis deux
    /// processus quand le backend est partagé — s'exécutent l'un après
    /// l'autre : le second voit l'écriture du premier. `update` peut être long
    /// (appel HTTP au provider) et ne doit pas rappeler le store. Une décision
    /// autre que [`CredentialUpdate::Keep`] sur une credential absente (`None`)
    /// est sans effet.
    fn update_locked(
        &self,
        user_id: &str,
        provider_id: &str,
        update: &mut dyn FnMut(Option<StoredCredential>) -> CredentialUpdate,
    ) -> Result<()>;

    /// Supprime les tokens. No-op si absents.
    fn delete(&self, user_id: &str, provider_id: &str) -> Result<()>;

    /// Liste les `provider_id` pour lesquels `user_id` a au moins un
    /// credential stocké. Tri lexical ascendant pour stabilité des tests.
    fn list_by_user(&self, user_id: &str) -> Result<Vec<String>>;

    /// Toutes les credentials de `user_id` (tokens + marque d'invalidation),
    /// triées par `provider_id`, en **une** lecture du backend (pas de N+1).
    /// Sans verrou : vue de lecture, pas de base à une décision de refresh.
    fn load_all_by_user(&self, user_id: &str) -> Result<Vec<(String, StoredCredential)>>;
}

/// Entrée de [`InMemoryConnectorTokenStore`] : enveloppe chiffrée + marque.
struct Entry {
    envelope: String,
    invalidated: Option<CredentialInvalidation>,
}

/// Impl in-process : HashMap sous Mutex, valeurs chiffrées via `EnvelopeCipher`.
///
/// [`update_locked`](ConnectorTokenStore::update_locked) garde le verrou de la
/// table entière pendant `update` : suffisant pour un process unique (dev,
/// tests), où le résolveur du worker sérialise déjà par clé.
pub struct InMemoryConnectorTokenStore<K: KeyProvider> {
    cipher: EnvelopeCipher<K>,
    inner: Mutex<HashMap<(String, String), Entry>>,
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
        let envelope = seal_tokens(&self.cipher, tokens)?;
        self.inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?
            .insert(
                (user_id.to_string(), provider_id.to_string()),
                Entry {
                    envelope,
                    invalidated: None,
                },
            );
        Ok(())
    }

    fn load(&self, user_id: &str, provider_id: &str) -> Result<Option<TokenSet>> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?;
        let Some(entry) = map.get(&(user_id.to_string(), provider_id.to_string())) else {
            return Ok(None);
        };
        open_tokens(&self.cipher, &entry.envelope).map(Some)
    }

    fn update_locked(
        &self,
        user_id: &str,
        provider_id: &str,
        update: &mut dyn FnMut(Option<StoredCredential>) -> CredentialUpdate,
    ) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?;
        let key = (user_id.to_string(), provider_id.to_string());
        let current = map
            .get(&key)
            .map(|entry| {
                Ok::<_, Error>(StoredCredential {
                    tokens: open_tokens(&self.cipher, &entry.envelope)?,
                    invalidated: entry.invalidated.clone(),
                })
            })
            .transpose()?;
        let decision = update(current);
        let Some(entry) = map.get_mut(&key) else {
            return Ok(());
        };
        match decision {
            CredentialUpdate::Keep => {}
            CredentialUpdate::Replace(tokens) => {
                *entry = Entry {
                    envelope: seal_tokens(&self.cipher, &tokens)?,
                    invalidated: None,
                };
            }
            CredentialUpdate::Invalidate(invalidation) => entry.invalidated = Some(invalidation),
        }
        Ok(())
    }

    fn delete(&self, user_id: &str, provider_id: &str) -> Result<()> {
        self.inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?
            .remove(&(user_id.to_string(), provider_id.to_string()));
        Ok(())
    }

    fn list_by_user(&self, user_id: &str) -> Result<Vec<String>> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?;
        let mut providers: Vec<String> = map
            .keys()
            .filter(|(u, _)| u == user_id)
            .map(|(_, p)| p.clone())
            .collect();
        providers.sort();
        Ok(providers)
    }

    fn load_all_by_user(&self, user_id: &str) -> Result<Vec<(String, StoredCredential)>> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("token store lock empoisonné".into()))?;
        let mut credentials = map
            .iter()
            .filter(|((u, _), _)| u == user_id)
            .map(|((_, provider), entry)| {
                Ok((
                    provider.clone(),
                    StoredCredential {
                        tokens: open_tokens(&self.cipher, &entry.envelope)?,
                        invalidated: entry.invalidated.clone(),
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        credentials.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(credentials)
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
    fn list_by_user_returns_sorted_provider_ids() {
        let store = make_store();
        store.store("alice", "slack", &sample_tokens()).unwrap();
        store.store("alice", "github", &sample_tokens()).unwrap();
        store.store("alice", "notion", &sample_tokens()).unwrap();
        store.store("bob", "google", &sample_tokens()).unwrap();

        let alice = store.list_by_user("alice").unwrap();
        assert_eq!(alice, vec!["github", "notion", "slack"]);
        let bob = store.list_by_user("bob").unwrap();
        assert_eq!(bob, vec!["google"]);
    }

    #[test]
    fn list_by_user_returns_empty_for_unknown_user() {
        let store = make_store();
        store.store("alice", "google", &sample_tokens()).unwrap();
        let result = store.list_by_user("charlie").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn seal_then_open_round_trips_and_hides_plaintext() {
        let cipher = EnvelopeCipher::new(InMemoryKeyProvider::with_key("k1", [3u8; 32]));
        let envelope = seal_tokens(&cipher, &sample_tokens()).unwrap();
        assert!(envelope.starts_with("v1:k1:"), "format envelope F6.3");
        assert!(!envelope.contains("at-1") && !envelope.contains("rt-1"));
        assert_eq!(open_tokens(&cipher, &envelope).unwrap(), sample_tokens());
    }

    #[test]
    fn open_with_another_key_fails() {
        let sealer = EnvelopeCipher::new(InMemoryKeyProvider::with_key("k1", [3u8; 32]));
        let other = EnvelopeCipher::new(InMemoryKeyProvider::with_key("k1", [4u8; 32]));
        let envelope = seal_tokens(&sealer, &sample_tokens()).unwrap();
        assert!(open_tokens(&other, &envelope).is_err());
    }

    #[test]
    fn stored_envelope_is_not_plaintext_at_rest() {
        let store = make_store();
        store.store("alice", "google", &sample_tokens()).unwrap();
        // Inspect le map interne — l'envelope ne contient pas `access_token` en clair.
        let inner = store.inner.lock().unwrap();
        let envelope = &inner.values().next().unwrap().envelope;
        assert!(envelope.starts_with("v1:k1:"), "format envelope F6.3");
        assert!(
            !envelope.contains("at-1"),
            "le token en clair NE DOIT PAS apparaître"
        );
    }

    fn locked(
        store: &InMemoryConnectorTokenStore<InMemoryKeyProvider>,
        decision: CredentialUpdate,
    ) -> Option<StoredCredential> {
        let mut seen = None;
        store
            .update_locked("alice", "google", &mut |current| {
                seen = current;
                decision.clone()
            })
            .unwrap();
        seen
    }

    fn invalidation() -> CredentialInvalidation {
        CredentialInvalidation {
            at_ms: 1_700_000_000_000,
            reason: "invalid_grant".into(),
        }
    }

    #[test]
    fn update_locked_sees_current_credential_and_keep_writes_nothing() {
        let store = make_store();
        assert_eq!(locked(&store, CredentialUpdate::Keep), None);
        store.store("alice", "google", &sample_tokens()).unwrap();
        let seen = locked(&store, CredentialUpdate::Keep).unwrap();
        assert_eq!(seen.tokens, sample_tokens());
        assert_eq!(seen.invalidated, None);
        assert_eq!(
            store.load("alice", "google").unwrap(),
            Some(sample_tokens())
        );
    }

    #[test]
    fn update_locked_replace_rewrites_encrypted_tokens() {
        let store = make_store();
        store.store("alice", "google", &sample_tokens()).unwrap();
        let refreshed = TokenSet {
            access_token: "at-2".into(),
            ..sample_tokens()
        };
        locked(&store, CredentialUpdate::Replace(refreshed.clone()));
        assert_eq!(store.load("alice", "google").unwrap(), Some(refreshed));
    }

    #[test]
    fn invalidation_is_kept_until_replace_or_reconnect() {
        let store = make_store();
        store.store("alice", "google", &sample_tokens()).unwrap();
        locked(&store, CredentialUpdate::Invalidate(invalidation()));
        let seen = locked(&store, CredentialUpdate::Keep).unwrap();
        assert_eq!(seen.invalidated, Some(invalidation()));
        assert_eq!(seen.tokens, sample_tokens(), "tokens conservés");

        // Rafraîchissement réussi : marque effacée.
        locked(&store, CredentialUpdate::Replace(sample_tokens()));
        assert_eq!(
            locked(&store, CredentialUpdate::Keep).unwrap().invalidated,
            None
        );

        // Reconnexion (`store`) : marque effacée aussi.
        locked(&store, CredentialUpdate::Invalidate(invalidation()));
        store.store("alice", "google", &sample_tokens()).unwrap();
        assert_eq!(
            locked(&store, CredentialUpdate::Keep).unwrap().invalidated,
            None
        );
    }

    #[test]
    fn load_all_by_user_returns_sorted_credentials_with_their_mark() {
        let store = make_store();
        store.store("alice", "slack", &sample_tokens()).unwrap();
        store.store("alice", "google", &sample_tokens()).unwrap();
        store.store("bob", "github", &sample_tokens()).unwrap();
        locked(&store, CredentialUpdate::Invalidate(invalidation()));

        let alice = store.load_all_by_user("alice").unwrap();
        let providers: Vec<&str> = alice.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(providers, vec!["google", "slack"]);
        assert_eq!(alice[0].1.invalidated, Some(invalidation()));
        assert_eq!(alice[0].1.tokens, sample_tokens());
        assert_eq!(alice[1].1.invalidated, None);
        assert!(store.load_all_by_user("carol").unwrap().is_empty());

        // Reconnexion : la marque disparaît de la lecture groupée aussi.
        store.store("alice", "google", &sample_tokens()).unwrap();
        assert_eq!(
            store.load_all_by_user("alice").unwrap()[0].1.invalidated,
            None
        );
    }

    #[test]
    fn update_locked_on_missing_credential_writes_nothing() {
        let store = make_store();
        locked(&store, CredentialUpdate::Replace(sample_tokens()));
        locked(&store, CredentialUpdate::Invalidate(invalidation()));
        assert!(store.is_empty());
    }
}

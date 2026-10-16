//! Endpoints des déclencheurs webhook (#202).
//!
//! Chaque nœud `trigger_type: "webhook"` d'un workflow reçoit une **URL
//! secrète** `/hooks/{token}`. Le token est dérivé par l'API d'une clé serveur
//! et d'un **sel** aléatoire propre à l'endpoint ; le store ne conserve que le
//! sel et le **hash** du token : la base seule ne permet de reconstituer
//! aucune URL, et une URL se résout par son hash.
//!
//! Un endpoint est identifié par `(propriétaire, workflow, nœud)`. Il est créé
//! à la demande ([`WebhookEndpointStore::ensure`]), remplacé par une rotation
//! (l'ancienne URL cesse aussitôt de fonctionner) et supprimé quand le nœud ou
//! le workflow disparaît ([`WebhookEndpointStore::retain`]).

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{Error, Result};

/// Cible d'une URL de webhook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEndpoint {
    /// Propriétaire du workflow (espace d'ids, #188) — et du run déclenché.
    pub owner_id: String,
    /// Identifiant du workflow dans l'espace du propriétaire.
    pub workflow_id: String,
    /// Nœud trigger webhook visé.
    pub node_id: String,
    /// Sel dont l'API dérive le token et le secret de signature.
    pub salt: Vec<u8>,
}

/// Secret d'un nouvel endpoint : sel aléatoire et hash du token qui en dérive.
#[derive(Clone, PartialEq, Eq)]
pub struct EndpointSecret {
    /// Sel aléatoire.
    pub salt: Vec<u8>,
    /// Hash du token (clé de résolution).
    pub token_hash: Vec<u8>,
}

impl std::fmt::Debug for EndpointSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EndpointSecret(<redacted>)")
    }
}

/// Stockage des endpoints de webhook.
#[async_trait]
pub trait WebhookEndpointStore: Send + Sync {
    /// Sel de l'endpoint du nœud `node_id`, créé avec `fresh` s'il n'existe
    /// pas encore. Deux appels concurrents renvoient le même sel.
    async fn ensure(
        &self,
        owner: &str,
        workflow_id: &str,
        node_id: &str,
        fresh: EndpointSecret,
    ) -> Result<Vec<u8>>;

    /// Remplace (ou crée) le secret de l'endpoint : l'ancien token ne se
    /// résout plus.
    async fn rotate(
        &self,
        owner: &str,
        workflow_id: &str,
        node_id: &str,
        fresh: EndpointSecret,
    ) -> Result<()>;

    /// Cible du token de hash `token_hash`, si elle existe.
    async fn resolve(&self, token_hash: &[u8]) -> Result<Option<WebhookEndpoint>>;

    /// Supprime les endpoints du workflow dont le nœud n'est pas dans `keep`
    /// (tous si `keep` est vide) ; renvoie le nombre supprimé.
    async fn retain(&self, owner: &str, workflow_id: &str, keep: &[String]) -> Result<usize>;
}

type Key = (String, String, String);

/// Implémentation en mémoire (dev, tests, mono-réplique).
#[derive(Debug, Default)]
pub struct InMemoryWebhookEndpointStore {
    endpoints: Mutex<BTreeMap<Key, EndpointSecret>>,
}

impl InMemoryWebhookEndpointStore {
    /// Store vide.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<Key, EndpointSecret>>> {
        self.endpoints
            .lock()
            .map_err(|_| Error::Invalid("webhook endpoints : verrou empoisonné".into()))
    }
}

fn key(owner: &str, workflow_id: &str, node_id: &str) -> Key {
    (owner.into(), workflow_id.into(), node_id.into())
}

#[async_trait]
impl WebhookEndpointStore for InMemoryWebhookEndpointStore {
    async fn ensure(
        &self,
        owner: &str,
        workflow_id: &str,
        node_id: &str,
        fresh: EndpointSecret,
    ) -> Result<Vec<u8>> {
        let mut endpoints = self.lock()?;
        Ok(endpoints
            .entry(key(owner, workflow_id, node_id))
            .or_insert(fresh)
            .salt
            .clone())
    }

    async fn rotate(
        &self,
        owner: &str,
        workflow_id: &str,
        node_id: &str,
        fresh: EndpointSecret,
    ) -> Result<()> {
        self.lock()?.insert(key(owner, workflow_id, node_id), fresh);
        Ok(())
    }

    async fn resolve(&self, token_hash: &[u8]) -> Result<Option<WebhookEndpoint>> {
        Ok(self
            .lock()?
            .iter()
            .find(|(_, secret)| secret.token_hash == token_hash)
            .map(|((owner, workflow, node), secret)| WebhookEndpoint {
                owner_id: owner.clone(),
                workflow_id: workflow.clone(),
                node_id: node.clone(),
                salt: secret.salt.clone(),
            }))
    }

    async fn retain(&self, owner: &str, workflow_id: &str, keep: &[String]) -> Result<usize> {
        let mut endpoints = self.lock()?;
        let before = endpoints.len();
        endpoints.retain(|(o, w, node), _| o != owner || w != workflow_id || keep.contains(node));
        Ok(before - endpoints.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(byte: u8) -> EndpointSecret {
        EndpointSecret {
            salt: vec![byte; 32],
            token_hash: vec![byte.wrapping_add(100); 32],
        }
    }

    #[tokio::test]
    async fn ensure_creates_once_then_returns_the_existing_salt() {
        let store = InMemoryWebhookEndpointStore::new();
        assert_eq!(
            store
                .ensure("alice", "wf", "hook", secret(1))
                .await
                .unwrap(),
            vec![1; 32]
        );
        assert_eq!(
            store
                .ensure("alice", "wf", "hook", secret(2))
                .await
                .unwrap(),
            vec![1; 32],
            "le second appel ne remplace pas le secret"
        );
        let target = store.resolve(&[101; 32]).await.unwrap().unwrap();
        assert_eq!(
            target,
            WebhookEndpoint {
                owner_id: "alice".into(),
                workflow_id: "wf".into(),
                node_id: "hook".into(),
                salt: vec![1; 32],
            }
        );
        assert!(store.resolve(&[102; 32]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rotation_invalidates_the_previous_token() {
        let store = InMemoryWebhookEndpointStore::new();
        store
            .ensure("alice", "wf", "hook", secret(1))
            .await
            .unwrap();
        store
            .rotate("alice", "wf", "hook", secret(2))
            .await
            .unwrap();
        assert!(store.resolve(&[101; 32]).await.unwrap().is_none());
        assert_eq!(
            store.resolve(&[102; 32]).await.unwrap().unwrap().salt,
            vec![2; 32]
        );
    }

    #[tokio::test]
    async fn endpoints_are_scoped_by_owner_and_workflow() {
        let store = InMemoryWebhookEndpointStore::new();
        store
            .ensure("alice", "wf", "hook", secret(1))
            .await
            .unwrap();
        assert_eq!(
            store.ensure("bob", "wf", "hook", secret(2)).await.unwrap(),
            vec![2; 32]
        );
        assert_eq!(
            store
                .ensure("alice", "wf2", "hook", secret(3))
                .await
                .unwrap(),
            vec![3; 32]
        );
    }

    #[tokio::test]
    async fn retain_drops_removed_nodes_of_that_workflow_only() {
        let store = InMemoryWebhookEndpointStore::new();
        store.ensure("alice", "wf", "a", secret(1)).await.unwrap();
        store.ensure("alice", "wf", "b", secret(2)).await.unwrap();
        store
            .ensure("alice", "other", "a", secret(3))
            .await
            .unwrap();
        store.ensure("bob", "wf", "a", secret(4)).await.unwrap();
        assert_eq!(store.retain("alice", "wf", &["a".into()]).await.unwrap(), 1);
        assert!(store.resolve(&[102; 32]).await.unwrap().is_none());
        assert!(store.resolve(&[101; 32]).await.unwrap().is_some());
        // Workflow supprimé : plus aucun endpoint.
        assert_eq!(store.retain("alice", "wf", &[]).await.unwrap(), 1);
        assert!(store.resolve(&[103; 32]).await.unwrap().is_some());
        assert!(store.resolve(&[104; 32]).await.unwrap().is_some());
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        assert_eq!(format!("{:?}", secret(7)), "EndpointSecret(<redacted>)");
    }
}

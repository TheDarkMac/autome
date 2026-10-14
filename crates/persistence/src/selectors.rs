//! Sélecteurs de backends pilotés par l'environnement (dette #107), partagés
//! par l'API et le worker : file (`AUTOME_QUEUE`) et stores
//! (`AUTOME_PERSISTENCE`). Évite de dupliquer la logique de choix
//! in-memory ↔ PostgreSQL/RabbitMQ entre les deux binaires.

use std::sync::Arc;

use autome_connectors::oauth::{ConnectorTokenStore, InMemoryConnectorTokenStore};
use autome_core::{
    Config, EnvelopeCipher, Error, InMemoryKeyProvider, InMemoryQueue, InMemoryRunStateStore,
    InMemoryWorkflowRepository, Queue, Result, RunStateStore, WorkflowRepository,
};
use sha2::{Digest, Sha256};

use crate::{connect, PgConnectorTokenStore, PgRunStateStore, PgWorkflowRepository, RabbitMqQueue};

/// Backend de persistance des stores (`AUTOME_PERSISTENCE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceMode {
    /// Stores in-process (défaut).
    Memory,
    /// Stores PostgreSQL.
    Postgres,
}

impl PersistenceMode {
    /// Lit le mode depuis `AUTOME_PERSISTENCE` (défaut : `Memory`).
    pub fn from_env() -> Result<Self> {
        Self::parse(std::env::var("AUTOME_PERSISTENCE").ok().as_deref())
    }

    /// Parse une valeur optionnelle (`None`/`""` ⇒ `Memory`).
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "" | "memory" | "in-memory" => Ok(Self::Memory),
            "postgres" | "postgresql" | "pg" => Ok(Self::Postgres),
            other => Err(Error::Invalid(format!(
                "AUTOME_PERSISTENCE invalide : `{other}` (attendu : memory | postgres)"
            ))),
        }
    }
}

/// Backend de file (`AUTOME_QUEUE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    /// File in-process (défaut).
    Memory,
    /// Broker RabbitMQ (`config.queue_url`).
    Rabbitmq,
}

impl QueueMode {
    /// Lit le mode depuis `AUTOME_QUEUE` (défaut : `Memory`).
    pub fn from_env() -> Result<Self> {
        match std::env::var("AUTOME_QUEUE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "memory" | "in-memory" => Ok(Self::Memory),
            "rabbitmq" | "rabbit" | "amqp" => Ok(Self::Rabbitmq),
            other => Err(Error::Invalid(format!(
                "AUTOME_QUEUE invalide : `{other}` (attendu : memory | rabbitmq)"
            ))),
        }
    }
}

/// Construit la file selon le `mode`. En `Rabbitmq`, se connecte au broker
/// (`config.queue_url`) — opération **async**.
pub async fn build_queue(config: &Config, mode: QueueMode) -> Result<Arc<dyn Queue + Send + Sync>> {
    match mode {
        QueueMode::Memory => Ok(Arc::new(InMemoryQueue::new())),
        QueueMode::Rabbitmq => Ok(Arc::new(RabbitMqQueue::connect(&config.queue_url).await?)),
    }
}

/// Construit le store d'état des runs selon le `mode`.
pub fn build_run_state_store(
    config: &Config,
    mode: PersistenceMode,
) -> Result<Arc<dyn RunStateStore + Send + Sync>> {
    Ok(match mode {
        PersistenceMode::Memory => Arc::new(InMemoryRunStateStore::new()),
        PersistenceMode::Postgres => Arc::new(PgRunStateStore::new(connect(&config.database_url)?)),
    })
}

/// Construit le repository de workflows selon le `mode`.
pub fn build_workflow_repository(
    config: &Config,
    mode: PersistenceMode,
) -> Result<Arc<dyn WorkflowRepository + Send + Sync>> {
    Ok(match mode {
        PersistenceMode::Memory => Arc::new(InMemoryWorkflowRepository::new()),
        PersistenceMode::Postgres => {
            Arc::new(PgWorkflowRepository::new(connect(&config.database_url)?))
        }
    })
}

/// Cipher des credentials de connecteurs : clé `v1` dérivée de
/// `config.encryption_key` (SHA-256 → 32 o). **Partagé par l'API (écriture au
/// callback OAuth) et le worker (lecture au run)** : les deux doivent recevoir
/// le même `ENCRYPTION_KEY`, sinon le worker ne peut pas déchiffrer.
pub fn connector_cipher(config: &Config) -> EnvelopeCipher<InMemoryKeyProvider> {
    let key: [u8; 32] = Sha256::digest(config.encryption_key.expose().as_bytes()).into();
    EnvelopeCipher::new(InMemoryKeyProvider::with_key("v1", key))
}

/// Construit le store des tokens OAuth de connecteurs selon le `mode`, chiffré
/// au repos par [`connector_cipher`]. En `Memory`, le store est propre au
/// process (non partagé API ↔ worker, perdu au redémarrage).
pub fn build_connector_token_store(
    config: &Config,
    mode: PersistenceMode,
) -> Result<Arc<dyn ConnectorTokenStore + Send + Sync>> {
    let cipher = connector_cipher(config);
    Ok(match mode {
        PersistenceMode::Memory => Arc::new(InMemoryConnectorTokenStore::new(cipher)),
        PersistenceMode::Postgres => Arc::new(PgConnectorTokenStore::new(
            connect(&config.database_url)?,
            cipher,
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_mode_parse() {
        assert_eq!(
            PersistenceMode::parse(None).unwrap(),
            PersistenceMode::Memory
        );
        assert_eq!(
            PersistenceMode::parse(Some("postgres")).unwrap(),
            PersistenceMode::Postgres
        );
        assert!(PersistenceMode::parse(Some("redis")).is_err());
    }

    #[tokio::test]
    async fn build_queue_memory_does_not_touch_network() {
        let config = Config::load(|_| None).unwrap();
        let _q = build_queue(&config, QueueMode::Memory).await.unwrap();
    }

    fn config_with_key(key: &'static str) -> Config {
        Config::load(move |k| (k == "ENCRYPTION_KEY").then(|| key.to_string())).unwrap()
    }

    fn sample_tokens() -> autome_connectors::oauth::TokenSet {
        autome_connectors::oauth::TokenSet {
            access_token: "xoxb-alice".into(),
            refresh_token: None,
            expires_at_ms: None,
            token_type: "Bearer".into(),
            scope: None,
        }
    }

    #[test]
    fn connector_cipher_is_shared_by_processes_with_the_same_key() {
        // L'API scelle, le worker (autre process, même ENCRYPTION_KEY) ouvre.
        let api = connector_cipher(&config_with_key("shared-key"));
        let worker = connector_cipher(&config_with_key("shared-key"));
        let envelope = autome_connectors::oauth::seal_tokens(&api, &sample_tokens()).unwrap();
        let opened = autome_connectors::oauth::open_tokens(&worker, &envelope).unwrap();
        assert_eq!(opened, sample_tokens());

        let other = connector_cipher(&config_with_key("another-key"));
        assert!(autome_connectors::oauth::open_tokens(&other, &envelope).is_err());
    }

    #[test]
    fn build_connector_token_store_memory_round_trips() {
        let config = Config::load(|_| None).unwrap();
        let store = build_connector_token_store(&config, PersistenceMode::Memory).unwrap();
        store.store("alice", "slack", &sample_tokens()).unwrap();
        assert_eq!(store.load("alice", "slack").unwrap(), Some(sample_tokens()));
        assert_eq!(store.load("bob", "slack").unwrap(), None);
    }

    #[test]
    fn build_connector_token_store_postgres_does_not_connect() {
        let config = Config::load(|k| {
            (k == "DATABASE_URL").then(|| "postgres://u:p@127.0.0.1:1/none".to_string())
        })
        .unwrap();
        let _store = build_connector_token_store(&config, PersistenceMode::Postgres).unwrap();
    }
}

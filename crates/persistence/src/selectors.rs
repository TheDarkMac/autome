//! Sélecteurs de backends pilotés par l'environnement (dette #107), partagés
//! par l'API et le worker : file (`AUTOME_QUEUE`) et stores
//! (`AUTOME_PERSISTENCE`). Évite de dupliquer la logique de choix
//! in-memory ↔ PostgreSQL/RabbitMQ entre les deux binaires.

use std::sync::Arc;

use autome_core::{
    Config, Error, InMemoryQueue, InMemoryRunStateStore, InMemoryWorkflowRepository, Queue, Result,
    RunStateStore, WorkflowRepository,
};

use crate::{connect, PgRunStateStore, PgWorkflowRepository, RabbitMqQueue};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_mode_parse() {
        assert_eq!(PersistenceMode::parse(None).unwrap(), PersistenceMode::Memory);
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
}

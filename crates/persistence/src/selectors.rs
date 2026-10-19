//! Sélecteurs de backends pilotés par l'environnement (dette #107), partagés
//! par l'API et le worker : file (`AUTOME_QUEUE`) et stores
//! (`AUTOME_PERSISTENCE`). Évite de dupliquer la logique de choix
//! in-memory ↔ PostgreSQL/RabbitMQ entre les deux binaires.

use std::sync::Arc;
use std::time::Duration;

use autome_connectors::oauth::{ConnectorTokenStore, InMemoryConnectorTokenStore};
use autome_core::{
    Config, EnvelopeCipher, Error, HealthCheck, InMemoryKeyProvider, InMemoryQueue,
    InMemoryRunStateStore, InMemoryTriggerScheduleStore, InMemoryWorkflowRepository, Profile,
    Queue, Result, RunStateStore, TriggerScheduleStore, WorkflowRepository,
};
use sha2::{Digest, Sha256};

use crate::{
    connect, PgConnectorTokenStore, PgRunStateStore, PgTriggerScheduleStore, PgWorkflowRepository,
    PostgresCheck, QueueCheck, RabbitMqQueue,
};

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

/// Refuse les backends **éphémères** en profil `prod` (API et worker).
///
/// En `memory`, chaque process a sa propre file et ses propres stores : l'API
/// publie les runs sur une file que le worker ne voit pas (runs bloqués en
/// `queued`), et comptes, workflows et tokens sont perdus au redémarrage et
/// jamais partagés entre répliques. Un `prod` en mémoire est donc toujours une
/// erreur de configuration ; le profil `dev` reste libre.
pub fn ensure_durable_backends(
    profile: Profile,
    persistence: PersistenceMode,
    queue: QueueMode,
) -> Result<()> {
    if profile != Profile::Prod {
        return Ok(());
    }
    let mut missing = Vec::new();
    if persistence == PersistenceMode::Memory {
        missing.push("AUTOME_PERSISTENCE=postgres");
    }
    if queue == QueueMode::Memory {
        missing.push("AUTOME_QUEUE=rabbitmq");
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "profil prod : backends in-memory refusés (état perdu au redémarrage, \
         non partagé entre l'API, le worker et leurs répliques) — positionner {}",
        missing.join(" et ")
    )))
}

/// Délai par défaut pour joindre le broker au démarrage.
const QUEUE_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Lit `AUTOME_QUEUE_CONNECT_TIMEOUT_SECS` (défaut 60 s).
fn queue_connect_timeout() -> Duration {
    std::env::var("AUTOME_QUEUE_CONNECT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(QUEUE_CONNECT_TIMEOUT)
}

/// Construit la file selon le `mode`. En `Rabbitmq`, se connecte au broker
/// (`config.queue_url`) — opération **async** —, en retentant les erreurs
/// réseau pendant `AUTOME_QUEUE_CONNECT_TIMEOUT_SECS` (défaut 60 s) : le
/// broker peut être démarré sans accepter encore les connexions.
pub async fn build_queue(config: &Config, mode: QueueMode) -> Result<Arc<dyn Queue + Send + Sync>> {
    build_consuming_queue(config, mode, crate::rabbitmq::DEFAULT_PREFETCH).await
}

/// [`build_queue`] pour un consommateur (#237) : en `Rabbitmq`, le broker
/// pousse jusqu'à `prefetch` messages non acquittés d'avance à
/// [`Queue::receive`] (le worker y met sa concurrence). Sans effet en mémoire.
pub async fn build_consuming_queue(
    config: &Config,
    mode: QueueMode,
    prefetch: u16,
) -> Result<Arc<dyn Queue + Send + Sync>> {
    match mode {
        QueueMode::Memory => Ok(Arc::new(InMemoryQueue::new())),
        QueueMode::Rabbitmq => {
            let queue =
                RabbitMqQueue::connect_retrying(&config.queue_url, queue_connect_timeout()).await?;
            Ok(Arc::new(queue.with_prefetch(prefetch)))
        }
    }
}

/// Vérifications de disponibilité des backends sélectionnés (#192) : la base
/// en `postgres` (pool dédié, sans connexion avant le premier appel), le
/// broker en `rabbitmq` (`queue` = la file du process). Aucune en mémoire.
pub fn build_health_checks(
    config: &Config,
    persistence: PersistenceMode,
    queue_mode: QueueMode,
    queue: Arc<dyn Queue + Send + Sync>,
) -> Result<Vec<Arc<dyn HealthCheck>>> {
    let mut checks: Vec<Arc<dyn HealthCheck>> = Vec::new();
    if persistence == PersistenceMode::Postgres {
        checks.push(Arc::new(PostgresCheck::new(connect(&config.database_url)?)));
    }
    if queue_mode == QueueMode::Rabbitmq {
        checks.push(Arc::new(QueueCheck::new(queue, "rabbitmq")));
    }
    Ok(checks)
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

/// Construit le store des échéances des déclencheurs planifiés (#227) selon
/// le `mode`. En `Memory`, il est propre au process : l'API et le worker ne le
/// partagent pas (aucun déclenchement planifié hors PostgreSQL).
pub fn build_trigger_schedule_store(
    config: &Config,
    mode: PersistenceMode,
) -> Result<Arc<dyn TriggerScheduleStore>> {
    Ok(match mode {
        PersistenceMode::Memory => Arc::new(InMemoryTriggerScheduleStore::new()),
        PersistenceMode::Postgres => {
            Arc::new(PgTriggerScheduleStore::new(connect(&config.database_url)?))
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

    #[test]
    fn dev_profile_accepts_in_memory_backends() {
        ensure_durable_backends(Profile::Dev, PersistenceMode::Memory, QueueMode::Memory).unwrap();
    }

    #[test]
    fn prod_profile_accepts_durable_backends() {
        ensure_durable_backends(
            Profile::Prod,
            PersistenceMode::Postgres,
            QueueMode::Rabbitmq,
        )
        .unwrap();
    }

    #[test]
    fn prod_profile_rejects_each_in_memory_backend_naming_the_variable() {
        let err =
            ensure_durable_backends(Profile::Prod, PersistenceMode::Memory, QueueMode::Rabbitmq)
                .unwrap_err()
                .to_string();
        assert!(err.contains("AUTOME_PERSISTENCE=postgres"), "{err}");
        assert!(!err.contains("AUTOME_QUEUE"), "{err}");

        let err =
            ensure_durable_backends(Profile::Prod, PersistenceMode::Postgres, QueueMode::Memory)
                .unwrap_err()
                .to_string();
        assert!(err.contains("AUTOME_QUEUE=rabbitmq"), "{err}");
        assert!(!err.contains("AUTOME_PERSISTENCE"), "{err}");

        let err =
            ensure_durable_backends(Profile::Prod, PersistenceMode::Memory, QueueMode::Memory)
                .unwrap_err()
                .to_string();
        assert!(
            err.contains("AUTOME_PERSISTENCE=postgres et AUTOME_QUEUE=rabbitmq"),
            "{err}"
        );
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
    fn health_checks_follow_the_selected_backends() {
        let config = Config::load(|_| None).unwrap();
        let queue: Arc<dyn Queue + Send + Sync> = Arc::new(InMemoryQueue::new());
        let names = |p, q| -> Vec<&'static str> {
            build_health_checks(&config, p, q, queue.clone())
                .unwrap()
                .iter()
                .map(|c| c.name())
                .collect()
        };
        assert!(names(PersistenceMode::Memory, QueueMode::Memory).is_empty());
        assert_eq!(
            names(PersistenceMode::Postgres, QueueMode::Rabbitmq),
            vec!["postgres", "rabbitmq"]
        );
        assert_eq!(
            names(PersistenceMode::Postgres, QueueMode::Memory),
            vec!["postgres"]
        );
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

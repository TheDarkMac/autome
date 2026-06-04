//! Sélection du backend de persistance au démarrage (dette #107, constat 2).
//!
//! Le mode est piloté par la variable d'environnement `AUTOME_PERSISTENCE` :
//! - `memory` (défaut) : stores in-process (dev, tests, single-node) ;
//! - `postgres` : stores adossés à PostgreSQL (`autome-persistence`), partagés
//!   entre répliques et persistés.
//!
//! En `postgres`, l'[`AuthState`] est bâti sur `PgRevocationStore` /
//! `PgPermissionStore` / `PgAuditSink` (pool unique partagé). Le `TokenService`
//! vient toujours de `config.jwt_secret` (constat 1).

use std::sync::Arc;

use autome_connectors::oauth::{InMemoryConnectorTokenStore, OAuthClient, OAuthRegistry};
use autome_connectors::{CredentialService, OAuthCredentialService, ReqwestHttpClient};
use autome_core::{
    Config, EnvelopeCipher, Error, InMemoryKeyProvider, InMemoryQueue, InMemoryRunStateStore,
    InMemoryWorkflowRepository, Queue, QueueRunDispatcher, Result, RunDispatcher, RunStateStore,
    WorkflowRepository,
};
use autome_persistence::{
    connect, run_migrations, PgAuditSink, PgPermissionStore, PgRevocationStore, PgRunStateStore,
    PgWorkflowRepository, RabbitMqQueue,
};
use sha2::{Digest, Sha256};

use crate::auth::{token_service_from_config, AuthState};

/// Backend de file retenu pour cette exécution (`AUTOME_QUEUE`).
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

/// Construit le service de credentials OAuth (dette #107, constat 3) :
/// transport HTTP **réel** ([`ReqwestHttpClient`]) + store de tokens **chiffré
/// au repos** avec une clé dérivée de `config.encryption_key` (SHA-256 → 32 o).
/// Le registre de providers est vide pour l'instant (config providers = futur).
fn build_credential_service(config: &Config) -> Arc<dyn CredentialService + Send + Sync> {
    let key: [u8; 32] = Sha256::digest(config.encryption_key.expose().as_bytes()).into();
    let keys = InMemoryKeyProvider::new();
    keys.add_key("v1", key, true).expect("clé de chiffrement");
    let store = Arc::new(InMemoryConnectorTokenStore::new(EnvelopeCipher::new(keys)));
    let oauth = OAuthClient::new(OAuthRegistry::new(), ReqwestHttpClient::new());
    Arc::new(OAuthCredentialService::new(oauth, store))
}

/// Backends applicatifs assemblés selon le [`PersistenceMode`] : un seul jeu
/// de stores partagé par tous les routers (auth + workflows + runs).
pub struct Backends {
    /// État d'authentification (token + révocations/permissions/audit).
    pub auth: AuthState,
    /// Repository des workflows.
    pub workflows: Arc<dyn WorkflowRepository + Send + Sync>,
    /// Store d'état des runs.
    pub runs: Arc<dyn RunStateStore + Send + Sync>,
    /// Dispatcher de runs (publie sur la file + checkpoint initial).
    pub dispatcher: Arc<dyn RunDispatcher + Send + Sync>,
    /// Service de credentials OAuth (transport HTTP réel + store chiffré).
    pub credentials: Arc<dyn CredentialService + Send + Sync>,
}

/// Assemble les backends. En `Postgres`, **un seul pool** est partagé par tous
/// les stores. La `queue` (construite par [`build_queue`] selon `AUTOME_QUEUE`)
/// est injectée — le dispatcher publie dessus.
pub fn build_backends(
    config: &Config,
    mode: PersistenceMode,
    queue: Arc<dyn Queue + Send + Sync>,
) -> Result<Backends> {
    match mode {
        PersistenceMode::Memory => {
            let runs: Arc<dyn RunStateStore + Send + Sync> = Arc::new(InMemoryRunStateStore::new());
            let dispatcher: Arc<dyn RunDispatcher + Send + Sync> =
                Arc::new(QueueRunDispatcher::new(queue, runs.clone()));
            Ok(Backends {
                auth: AuthState::from_config(config),
                workflows: Arc::new(InMemoryWorkflowRepository::new()),
                runs,
                dispatcher,
                credentials: build_credential_service(config),
            })
        }
        PersistenceMode::Postgres => {
            let pool = connect(&config.database_url)?;
            let auth = AuthState::new(
                token_service_from_config(config),
                Arc::new(PgRevocationStore::new(pool.clone())),
                Arc::new(PgPermissionStore::new(pool.clone())),
                Arc::new(PgAuditSink::new(pool.clone())),
            );
            let runs: Arc<dyn RunStateStore + Send + Sync> =
                Arc::new(PgRunStateStore::new(pool.clone()));
            let dispatcher: Arc<dyn RunDispatcher + Send + Sync> =
                Arc::new(QueueRunDispatcher::new(queue, runs.clone()));
            Ok(Backends {
                auth,
                workflows: Arc::new(PgWorkflowRepository::new(pool)),
                runs,
                dispatcher,
                credentials: build_credential_service(config),
            })
        }
    }
}

/// `true` si `AUTOME_MIGRATE` demande d'appliquer les migrations au démarrage
/// (`1`/`true`/`yes`, insensible à la casse).
pub fn migrate_on_boot_requested() -> bool {
    matches!(
        std::env::var("AUTOME_MIGRATE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

/// Applique les migrations sur la base de `config` (mode `postgres`). Renvoie
/// le nombre de migrations appliquées. Échoue si la base est injoignable.
pub async fn migrate(config: &Config) -> Result<usize> {
    let pool = connect(&config.database_url)?;
    run_migrations(&pool).await
}

/// Backend de persistance retenu pour cette exécution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceMode {
    /// Stores in-process (défaut).
    Memory,
    /// Stores PostgreSQL (`autome-persistence`).
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

/// Construit l'[`AuthState`] selon le `mode` :
/// - `Memory` : [`AuthState::from_config`] (in-memory) ;
/// - `Postgres` : pool partagé + stores PG. Le pool n'ouvre pas de connexion à
///   la construction (deadpool connecte à la demande) — les erreurs DB
///   remontent au premier usage.
pub fn build_auth_state(config: &Config, mode: PersistenceMode) -> Result<AuthState> {
    match mode {
        PersistenceMode::Memory => Ok(AuthState::from_config(config)),
        PersistenceMode::Postgres => {
            let pool = connect(&config.database_url)?;
            Ok(AuthState::new(
                token_service_from_config(config),
                Arc::new(PgRevocationStore::new(pool.clone())),
                Arc::new(PgPermissionStore::new(pool.clone())),
                // Audit persistant en base ; passer à NoopAuditSink si l'on
                // veut explicitement désactiver l'audit en prod.
                Arc::new(PgAuditSink::new(pool)),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_to_memory() {
        assert_eq!(
            PersistenceMode::parse(None).unwrap(),
            PersistenceMode::Memory
        );
        assert_eq!(
            PersistenceMode::parse(Some("")).unwrap(),
            PersistenceMode::Memory
        );
        assert_eq!(
            PersistenceMode::parse(Some("memory")).unwrap(),
            PersistenceMode::Memory
        );
    }

    #[test]
    fn parse_postgres_variants() {
        for v in ["postgres", "Postgres", "POSTGRESQL", " pg "] {
            assert_eq!(
                PersistenceMode::parse(Some(v)).unwrap(),
                PersistenceMode::Postgres,
                "valeur: {v}"
            );
        }
    }

    #[test]
    fn parse_rejects_unknown() {
        let err = PersistenceMode::parse(Some("redis")).unwrap_err();
        assert!(matches!(err, Error::Invalid(m) if m.contains("AUTOME_PERSISTENCE")));
    }

    #[test]
    fn memory_mode_builds_in_memory_auth_state() {
        let config = Config::load(|_| None).expect("config dev");
        // Ne doit pas toucher au réseau ni échouer.
        let _state = build_auth_state(&config, PersistenceMode::Memory).unwrap();
    }

    #[test]
    fn postgres_mode_builds_without_connecting() {
        // `build_unchecked` ⇒ la construction n'ouvre pas de connexion ; on peut
        // bâtir l'état avec un DSN qui ne répond pas (les erreurs viendraient au
        // premier usage, pas ici).
        let config = Config::load(|k| match k {
            "APP_PROFILE" => Some("prod".into()),
            "DATABASE_URL" => Some("postgres://u:p@127.0.0.1:1/none".into()),
            "JWT_SECRET" => Some("prod-secret-32-characters-minimum!!".into()),
            "ENCRYPTION_KEY" => Some("prod-encryption-key".into()),
            _ => None,
        })
        .expect("config prod");
        let _state = build_auth_state(&config, PersistenceMode::Postgres).unwrap();
    }
}

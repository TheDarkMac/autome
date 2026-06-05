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
    Config, EnvelopeCipher, InMemoryKeyProvider, InMemoryRunStateStore, InMemoryWorkflowRepository,
    Queue, QueueRunDispatcher, Result, RunDispatcher, RunStateStore, WorkflowRepository,
};
use autome_persistence::{
    connect, run_migrations, PgAuditSink, PgPermissionStore, PgRevocationStore, PgRunStateStore,
    PgWorkflowRepository,
};
use sha2::{Digest, Sha256};

use crate::auth::{token_service_from_config, AuthState};

// Sélecteurs de file/stores partagés avec le worker (dette #107).
pub use autome_persistence::{build_queue, PersistenceMode, QueueMode};

/// Construit le service de credentials OAuth (dette #107, constat 3) :
/// transport HTTP **réel** ([`ReqwestHttpClient`]) + store de tokens **chiffré
/// au repos** avec une clé dérivée de `config.encryption_key` (SHA-256 → 32 o).
/// Le registre de providers est peuplé au démarrage depuis l'environnement
/// (`AUTOME_OAUTH_PROVIDERS` + `AUTOME_OAUTH_<ID>_*`, #161) — vide en l'absence
/// de config (OAuth alors indisponible) ; une config incomplète échoue (fail-fast).
fn build_credential_service(config: &Config) -> Result<Arc<dyn CredentialService + Send + Sync>> {
    let key: [u8; 32] = Sha256::digest(config.encryption_key.expose().as_bytes()).into();
    let keys = InMemoryKeyProvider::new();
    keys.add_key("v1", key, true).expect("clé de chiffrement");
    let store = Arc::new(InMemoryConnectorTokenStore::new(EnvelopeCipher::new(keys)));
    let oauth = OAuthClient::new(OAuthRegistry::from_env()?, ReqwestHttpClient::new());
    Ok(Arc::new(OAuthCredentialService::new(oauth, store)))
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
                credentials: build_credential_service(config)?,
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
                credentials: build_credential_service(config)?,
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

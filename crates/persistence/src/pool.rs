//! Pool de connexions PostgreSQL **async** (deadpool + tokio-postgres).

use autome_core::{Error, Result};
use deadpool_postgres::{Config as DeadpoolConfig, Pool, Runtime};
use tokio_postgres::NoTls;

/// Pool de connexions PostgreSQL partageable (`Clone`, `Send + Sync`).
///
/// `NoTls` : suffisant pour un trafic intra-cluster (services ↔ Postgres dans
/// le même namespace k8s). Le chiffrement du lien pourra être ajouté plus tard.
pub type PgPool = Pool;

/// Construit un pool à partir d'un DSN (`postgres://user:pass@host/db`).
///
/// La connexion n'est pas ouverte ici : deadpool établit les connexions à la
/// demande (`pool.get().await`). Les erreurs de connexion remontent au premier
/// usage, avec un message clair.
pub fn connect(database_url: &str) -> Result<PgPool> {
    let cfg = DeadpoolConfig {
        url: Some(database_url.to_string()),
        ..DeadpoolConfig::new()
    };
    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| Error::Invalid(format!("storage: création du pool PostgreSQL : {e}")))
}

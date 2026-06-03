//! Pool de connexions PostgreSQL (r2d2 + driver bloquant `postgres`).

use autome_core::{Error, Result};
use r2d2_postgres::PostgresConnectionManager;

/// Pool de connexions PostgreSQL partageable (`Clone`, `Send + Sync`).
///
/// `NoTls` : suffisant pour un trafic intra-cluster (services ↔ Postgres dans
/// le même namespace k8s). Le chiffrement du lien (TLS / sslmode) pourra être
/// ajouté ultérieurement pour les déploiements multi-réseaux.
pub type PgPool = r2d2::Pool<PostgresConnectionManager<postgres::NoTls>>;

/// Construit un pool à partir d'un DSN PostgreSQL (`postgres://user:pass@host/db`).
///
/// La connexion n'est **pas** testée à la construction (`build_unchecked`) :
/// le démarrage du service ne dépend pas de la disponibilité immédiate de la
/// base ; les erreurs remontent au premier usage, avec un message clair.
pub fn connect(database_url: &str) -> Result<PgPool> {
    let manager = PostgresConnectionManager::new(
        database_url
            .parse()
            .map_err(|e| Error::Invalid(format!("storage: DSN PostgreSQL invalide : {e}")))?,
        postgres::NoTls,
    );
    Ok(r2d2::Pool::builder().build_unchecked(manager))
}

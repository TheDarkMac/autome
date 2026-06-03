//! [`RevocationStore`] adossé à PostgreSQL (table `revoked_tokens`), **async**.
//!
//! Persiste la liste de révocation des JWT (par `jti`) — survit aux
//! redémarrages et est partagée entre répliques (dette #107, constat 2).
//! Schéma : migration `migrations/0006_auth_revocations.sql`.

use async_trait::async_trait;
use autome_core::{Error, RevocationStore};

use crate::pool::PgPool;

/// Conversion uniforme des erreurs deadpool/tokio-postgres en `core::Error`.
fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Liste de révocation persistée dans PostgreSQL.
#[derive(Clone)]
pub struct PgRevocationStore {
    pool: PgPool,
}

impl PgRevocationStore {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RevocationStore for PgRevocationStore {
    async fn is_revoked(&self, jti: &str) -> autome_core::Result<bool> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt("SELECT 1 FROM revoked_tokens WHERE jti = $1", &[&jti])
            .await
            .map_err(|e| storage_err("is_revoked", e))?;
        Ok(row.is_some())
    }

    async fn revoke(&self, jti: &str, exp_ms: i64) -> autome_core::Result<()> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Idempotent : on conserve le `exp_ms` le plus tardif (cohérent avec
        // l'impl in-memory qui ne raccourcit jamais une révocation).
        client
            .execute(
                "INSERT INTO revoked_tokens (jti, exp_ms) VALUES ($1, $2) \
                 ON CONFLICT (jti) DO UPDATE \
                 SET exp_ms = GREATEST(revoked_tokens.exp_ms, EXCLUDED.exp_ms)",
                &[&jti, &exp_ms],
            )
            .await
            .map_err(|e| storage_err("revoke", e))?;
        Ok(())
    }

    async fn gc(&self, now_ms: i64) -> autome_core::Result<usize> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute("DELETE FROM revoked_tokens WHERE exp_ms <= $1", &[&now_ms])
            .await
            .map_err(|e| storage_err("gc", e))?;
        Ok(n as usize)
    }
}

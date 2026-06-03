//! [`RevocationStore`] adossé à PostgreSQL (table `revoked_tokens`).
//!
//! Persiste la liste de révocation des JWT (par `jti`) — contrairement à
//! `InMemoryRevocationStore`, elle survit aux redémarrages et est **partagée
//! entre répliques** (correction de la dette #107, constat 2).
//!
//! Schéma : migration `migrations/0006_auth_revocations.sql`.

use autome_core::{Error, RevocationStore};

use crate::pool::PgPool;

/// Conversion uniforme des erreurs r2d2/postgres en `core::Error`.
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

impl RevocationStore for PgRevocationStore {
    fn is_revoked(&self, jti: &str) -> autome_core::Result<bool> {
        let mut client = self.pool.get().map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt("SELECT 1 FROM revoked_tokens WHERE jti = $1", &[&jti])
            .map_err(|e| storage_err("is_revoked", e))?;
        Ok(row.is_some())
    }

    fn revoke(&self, jti: &str, exp_ms: i64) -> autome_core::Result<()> {
        let mut client = self.pool.get().map_err(|e| storage_err("pool", e))?;
        // Idempotent : on conserve le `exp_ms` le plus tardif (cohérent avec
        // l'impl in-memory qui ne raccourcit jamais une révocation).
        client
            .execute(
                "INSERT INTO revoked_tokens (jti, exp_ms) VALUES ($1, $2) \
                 ON CONFLICT (jti) DO UPDATE \
                 SET exp_ms = GREATEST(revoked_tokens.exp_ms, EXCLUDED.exp_ms)",
                &[&jti, &exp_ms],
            )
            .map_err(|e| storage_err("revoke", e))?;
        Ok(())
    }

    fn gc(&self, now_ms: i64) -> autome_core::Result<usize> {
        let mut client = self.pool.get().map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute("DELETE FROM revoked_tokens WHERE exp_ms <= $1", &[&now_ms])
            .map_err(|e| storage_err("gc", e))?;
        Ok(n as usize)
    }
}

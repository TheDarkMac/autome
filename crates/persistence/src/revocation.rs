//! [`RevocationStore`] adossé à PostgreSQL (table `revoked_tokens`), **async**.
//!
//! Persiste la liste de révocation des JWT (par `jti`) — survit aux
//! redémarrages et est partagée entre répliques (dette #107, constat 2).
//! Schéma : migrations `0006_auth_revocations.sql` (révocations par `jti`)
//! et `0013_session_versions.sql` (versions de session par utilisateur, #186).

use async_trait::async_trait;
use autome_core::{Error, RevocationStore, TokenState};

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

    async fn revoke_once(&self, jti: &str, exp_ms: i64) -> autome_core::Result<bool> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Une seule instruction : la contrainte de clé primaire départage les
        // appels concurrents, y compris d'une réplique à l'autre (#184).
        let inserted = client
            .execute(
                "INSERT INTO revoked_tokens (jti, exp_ms) VALUES ($1, $2) \
                 ON CONFLICT (jti) DO NOTHING",
                &[&jti, &exp_ms],
            )
            .await
            .map_err(|e| storage_err("revoke_once", e))?;
        Ok(inserted == 1)
    }

    async fn session_version(&self, user_id: &str) -> autome_core::Result<i64> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                "SELECT version FROM session_versions WHERE user_id = $1",
                &[&user_id],
            )
            .await
            .map_err(|e| storage_err("session_version", e))?;
        Ok(row.map_or(0, |r| r.get(0)))
    }

    async fn bump_session_version(&self, user_id: &str) -> autome_core::Result<i64> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Upsert en une instruction : le verrou de ligne sérialise les
        // incréments concurrents, y compris d'une réplique à l'autre.
        let row = client
            .query_one(
                "INSERT INTO session_versions (user_id, version) VALUES ($1, 1) \
                 ON CONFLICT (user_id) DO UPDATE \
                 SET version = session_versions.version + 1 \
                 RETURNING version",
                &[&user_id],
            )
            .await
            .map_err(|e| storage_err("bump_session_version", e))?;
        Ok(row.get(0))
    }

    async fn token_state(&self, jti: &str, user_id: &str) -> autome_core::Result<TokenState> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Un seul aller-retour par requête authentifiée, comme avant #186.
        let row = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM revoked_tokens WHERE jti = $1), \
                        COALESCE((SELECT version FROM session_versions WHERE user_id = $2), 0)",
                &[&jti, &user_id],
            )
            .await
            .map_err(|e| storage_err("token_state", e))?;
        Ok(TokenState {
            revoked: row.get(0),
            session_version: row.get(1),
        })
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

//! [`PermissionStore`] adossé à PostgreSQL (table `user_permissions`).
//!
//! Persiste les permissions par utilisateur (partagées entre répliques),
//! contrairement à `InMemoryPermissionStore`. Schéma : migration
//! `migrations/0007_rbac_permissions.sql`.

use std::collections::HashSet;

use autome_core::{Error, PermissionStore};

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Permissions par utilisateur, persistées dans PostgreSQL.
#[derive(Clone)]
pub struct PgPermissionStore {
    pool: PgPool,
}

impl PgPermissionStore {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Attribue une permission à `user_id`. Idempotent.
    pub fn grant(&self, user_id: &str, permission: &str) -> autome_core::Result<()> {
        let mut client = self.pool.get().map_err(|e| storage_err("pool", e))?;
        client
            .execute(
                "INSERT INTO user_permissions (user_id, permission) VALUES ($1, $2) \
                 ON CONFLICT (user_id, permission) DO NOTHING",
                &[&user_id, &permission],
            )
            .map_err(|e| storage_err("grant", e))?;
        Ok(())
    }

    /// Retire une permission. No-op si elle n'était pas accordée.
    pub fn revoke(&self, user_id: &str, permission: &str) -> autome_core::Result<()> {
        let mut client = self.pool.get().map_err(|e| storage_err("pool", e))?;
        client
            .execute(
                "DELETE FROM user_permissions WHERE user_id = $1 AND permission = $2",
                &[&user_id, &permission],
            )
            .map_err(|e| storage_err("revoke", e))?;
        Ok(())
    }
}

impl PermissionStore for PgPermissionStore {
    fn permissions_for(&self, user_id: &str) -> autome_core::Result<HashSet<String>> {
        let mut client = self.pool.get().map_err(|e| storage_err("pool", e))?;
        let rows = client
            .query(
                "SELECT permission FROM user_permissions WHERE user_id = $1",
                &[&user_id],
            )
            .map_err(|e| storage_err("permissions_for", e))?;
        Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
    }
}

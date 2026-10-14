//! [`UserStore`] adossé à PostgreSQL (tables `users` + `user_permissions`), **async**.
//!
//! Schéma : migrations `0001_users.sql` et `0007_rbac_permissions.sql`. Le
//! compte et ses permissions sont écrits dans **une seule transaction** : un
//! crash entre les deux ne laisse ni compte sans droits ni droits orphelins.

use std::collections::HashSet;

use async_trait::async_trait;
use autome_core::{CreateUser, Error, UserRecord, UserStore};

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Comptes utilisateurs persistés dans PostgreSQL.
#[derive(Clone)]
pub struct PgUserStore {
    pool: PgPool,
}

impl PgUserStore {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UserStore for PgUserStore {
    async fn create(
        &self,
        email: &str,
        password_hash: &str,
        permissions: &HashSet<String>,
    ) -> autome_core::Result<CreateUser> {
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // `ON CONFLICT DO NOTHING RETURNING` : aucune ligne ⇒ email déjà pris,
        // détecté sans dépendre du code d'erreur 23505.
        let row = tx
            .query_opt(
                "INSERT INTO users (email, password_hash) VALUES ($1, $2) \
                 ON CONFLICT (email) DO NOTHING RETURNING id::text",
                &[&email, &password_hash],
            )
            .await
            .map_err(|e| storage_err("create user", e))?;
        let Some(row) = row else {
            return Ok(CreateUser::EmailTaken);
        };
        let id: String = row.get(0);

        for permission in permissions {
            tx.execute(
                "INSERT INTO user_permissions (user_id, permission) VALUES ($1, $2) \
                 ON CONFLICT (user_id, permission) DO NOTHING",
                &[&id, permission],
            )
            .await
            .map_err(|e| storage_err("grant", e))?;
        }
        tx.commit().await.map_err(|e| storage_err("commit", e))?;

        Ok(CreateUser::Created(UserRecord {
            id,
            email: email.to_string(),
            password_hash: password_hash.to_string(),
        }))
    }

    async fn find_by_email(&self, email: &str) -> autome_core::Result<Option<UserRecord>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                "SELECT id::text, email, password_hash FROM users WHERE email = $1",
                &[&email],
            )
            .await
            .map_err(|e| storage_err("find_by_email", e))?;
        Ok(row.map(|r| UserRecord {
            id: r.get(0),
            email: r.get(1),
            password_hash: r.get(2),
        }))
    }
}

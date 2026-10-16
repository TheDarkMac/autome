//! [`WebhookEndpointStore`] adossé à PostgreSQL (table `webhook_endpoints`,
//! #202).
//!
//! Ne contient que le sel et le SHA-256 du token de chaque endpoint (cf.
//! `autome_core::webhook`). La résolution d'une requête entrante est une
//! lecture par l'index unique de `token_hash`. Les endpoints d'un workflow
//! supprimé disparaissent par cascade.
//! Schéma : migration `migrations/0016_webhook_endpoints.sql`.

use async_trait::async_trait;
use autome_core::{EndpointSecret, Error, Result, WebhookEndpoint, WebhookEndpointStore};

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Endpoints de webhook persistés dans PostgreSQL.
#[derive(Clone)]
pub struct PgWebhookEndpointStore {
    pool: PgPool,
}

impl PgWebhookEndpointStore {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WebhookEndpointStore for PgWebhookEndpointStore {
    async fn ensure(
        &self,
        owner: &str,
        workflow_id: &str,
        node_id: &str,
        fresh: EndpointSecret,
    ) -> Result<Vec<u8>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // `DO UPDATE` sans effet plutôt que `DO NOTHING` : la ligne existante
        // est renvoyée même si elle vient d'être insérée par une transaction
        // concurrente (le verrou de ligne attend son commit).
        let row = client
            .query_one(
                "INSERT INTO webhook_endpoints (owner_id, workflow_id, node_id, salt, token_hash) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (owner_id, workflow_id, node_id) \
                 DO UPDATE SET salt = webhook_endpoints.salt \
                 RETURNING salt",
                &[
                    &owner,
                    &workflow_id,
                    &node_id,
                    &fresh.salt,
                    &fresh.token_hash,
                ],
            )
            .await
            .map_err(|e| storage_err("webhook_endpoints.ensure", e))?;
        Ok(row.get(0))
    }

    async fn rotate(
        &self,
        owner: &str,
        workflow_id: &str,
        node_id: &str,
        fresh: EndpointSecret,
    ) -> Result<()> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        client
            .execute(
                "INSERT INTO webhook_endpoints (owner_id, workflow_id, node_id, salt, token_hash) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (owner_id, workflow_id, node_id) \
                 DO UPDATE SET salt = EXCLUDED.salt, token_hash = EXCLUDED.token_hash",
                &[
                    &owner,
                    &workflow_id,
                    &node_id,
                    &fresh.salt,
                    &fresh.token_hash,
                ],
            )
            .await
            .map_err(|e| storage_err("webhook_endpoints.rotate", e))?;
        Ok(())
    }

    async fn resolve(&self, token_hash: &[u8]) -> Result<Option<WebhookEndpoint>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                "SELECT owner_id, workflow_id, node_id, salt FROM webhook_endpoints \
                 WHERE token_hash = $1",
                &[&token_hash],
            )
            .await
            .map_err(|e| storage_err("webhook_endpoints.resolve", e))?;
        Ok(row.map(|row| WebhookEndpoint {
            owner_id: row.get(0),
            workflow_id: row.get(1),
            node_id: row.get(2),
            salt: row.get(3),
        }))
    }

    async fn retain(&self, owner: &str, workflow_id: &str, keep: &[String]) -> Result<usize> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let removed = client
            .execute(
                "DELETE FROM webhook_endpoints \
                 WHERE owner_id = $1 AND workflow_id = $2 AND node_id <> ALL($3)",
                &[&owner, &workflow_id, &keep],
            )
            .await
            .map_err(|e| storage_err("webhook_endpoints.retain", e))?;
        Ok(removed as usize)
    }
}

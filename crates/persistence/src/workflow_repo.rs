//! [`WorkflowRepository`] adossé à PostgreSQL (tables `workflows` +
//! `workflow_versions`), **async**.
//!
//! Le `WorkflowGraph` complet est sérialisé en `jsonb` dans
//! `workflow_versions.graph` ; `workflows.current_version` pointe la version
//! servie par `get`/`list`. `update` publie une nouvelle version (historique
//! conservé). IDs logiques en TEXT (migration `0009`).

use async_trait::async_trait;
use autome_core::{Error, Pagination, WorkflowGraph, WorkflowId, WorkflowPage, WorkflowRepository};

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Repository de workflows persisté dans PostgreSQL.
#[derive(Clone)]
pub struct PgWorkflowRepository {
    pool: PgPool,
}

impl PgWorkflowRepository {
    /// Construit le repository sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WorkflowRepository for PgWorkflowRepository {
    async fn create(
        &self,
        workflow: WorkflowGraph,
        owner: Option<&str>,
    ) -> autome_core::Result<()> {
        let id = workflow.id.0.clone();
        let graph = serde_json::to_value(&workflow)?;
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // `ON CONFLICT DO NOTHING` + contrôle du nombre de lignes : permet de
        // détecter le doublon sans 23505 brut.
        let inserted = tx
            .execute(
                "INSERT INTO workflows (id, name, current_version, owner_id) \
                 VALUES ($1, $2, 1, $3) ON CONFLICT (id) DO NOTHING",
                &[&id, &workflow.name, &owner],
            )
            .await
            .map_err(|e| storage_err("create", e))?;
        if inserted == 0 {
            return Err(Error::Invalid(format!("workflow déjà existant : {id}")));
        }
        tx.execute(
            "INSERT INTO workflow_versions (workflow_id, version, graph) VALUES ($1, 1, $2)",
            &[&id, &graph],
        )
        .await
        .map_err(|e| storage_err("create version", e))?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    async fn get(&self, id: &WorkflowId) -> autome_core::Result<Option<WorkflowGraph>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                "SELECT v.graph FROM workflows w \
                 JOIN workflow_versions v \
                   ON v.workflow_id = w.id AND v.version = w.current_version \
                 WHERE w.id = $1",
                &[&id.0],
            )
            .await
            .map_err(|e| storage_err("get", e))?;
        match row {
            None => Ok(None),
            Some(r) => {
                let graph: serde_json::Value = r.get(0);
                Ok(Some(serde_json::from_value(graph)?))
            }
        }
    }

    async fn owner_of(&self, id: &WorkflowId) -> autome_core::Result<Option<String>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt("SELECT owner_id FROM workflows WHERE id = $1", &[&id.0])
            .await
            .map_err(|e| storage_err("owner_of", e))?;
        Ok(row.and_then(|r| r.get::<_, Option<String>>(0)))
    }

    async fn list(
        &self,
        owner: Option<&str>,
        paging: Pagination,
    ) -> autome_core::Result<WorkflowPage> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // `$1 IS NULL` ⇒ pas de filtre propriétaire (super-admin).
        let total: i64 = client
            .query_one(
                "SELECT count(*) FROM workflows WHERE ($1::text IS NULL OR owner_id = $1)",
                &[&owner],
            )
            .await
            .map_err(|e| storage_err("list count", e))?
            .get(0);
        let limit = paging.limit as i64;
        let offset = paging.offset as i64;
        let rows = client
            .query(
                "SELECT v.graph FROM workflows w \
                 JOIN workflow_versions v \
                   ON v.workflow_id = w.id AND v.version = w.current_version \
                 WHERE ($1::text IS NULL OR w.owner_id = $1) \
                 ORDER BY w.id LIMIT $2 OFFSET $3",
                &[&owner, &limit, &offset],
            )
            .await
            .map_err(|e| storage_err("list", e))?;
        let items = rows
            .iter()
            .map(|r| serde_json::from_value(r.get::<_, serde_json::Value>(0)).map_err(Error::from))
            .collect::<autome_core::Result<Vec<WorkflowGraph>>>()?;
        Ok(WorkflowPage {
            items,
            total: total as usize,
        })
    }

    async fn update(&self, workflow: WorkflowGraph) -> autome_core::Result<()> {
        let id = workflow.id.0.clone();
        let graph = serde_json::to_value(&workflow)?;
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // Version suivante = current_version + 1 (historique conservé).
        let next: Option<i32> = tx
            .query_opt(
                "SELECT current_version + 1 FROM workflows WHERE id = $1",
                &[&id],
            )
            .await
            .map_err(|e| storage_err("update lookup", e))?
            .map(|r| r.get(0));
        let Some(next) = next else {
            return Err(Error::Invalid(format!("workflow inconnu : {id}")));
        };
        tx.execute(
            "INSERT INTO workflow_versions (workflow_id, version, graph) VALUES ($1, $2, $3)",
            &[&id, &next, &graph],
        )
        .await
        .map_err(|e| storage_err("update version", e))?;
        tx.execute(
            "UPDATE workflows SET name = $2, current_version = $3 WHERE id = $1",
            &[&id, &workflow.name, &next],
        )
        .await
        .map_err(|e| storage_err("update", e))?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    async fn delete(&self, id: &WorkflowId) -> autome_core::Result<bool> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute("DELETE FROM workflows WHERE id = $1", &[&id.0])
            .await
            .map_err(|e| storage_err("delete", e))?;
        Ok(n > 0)
    }
}

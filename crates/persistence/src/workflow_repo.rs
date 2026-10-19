//! [`WorkflowRepository`] adossé à PostgreSQL (tables `workflows` +
//! `workflow_versions`), **async**.
//!
//! Le `WorkflowGraph` complet est sérialisé en `jsonb` dans
//! `workflow_versions.graph` ; `workflows.current_version` pointe la version
//! servie par `get`/`list`. `update` publie une nouvelle version (historique
//! conservé). IDs logiques en TEXT (migration `0009`), namespacés par
//! propriétaire : clé `(owner_id, id)` (migration `0014`, #188).
//! `list_all` (#243) parcourt tous les espaces dans l'ordre de cette clé ;
//! il lit le nom dans `workflows.name` et ne renvoie du graphe que le nombre
//! de nœuds.

use async_trait::async_trait;
use autome_core::{
    Error, Pagination, WorkflowFilter, WorkflowGraph, WorkflowId, WorkflowListing,
    WorkflowListingPage, WorkflowPage, WorkflowRepository,
};

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
    async fn create(&self, owner: &str, workflow: WorkflowGraph) -> autome_core::Result<()> {
        let id = workflow.id.0.clone();
        let graph = serde_json::to_value(&workflow)?;
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // Conflit seulement dans l'espace de `owner` (clé `(owner_id, id)`).
        let inserted = tx
            .execute(
                "INSERT INTO workflows (owner_id, id, name, current_version) \
                 VALUES ($1, $2, $3, 1) ON CONFLICT (owner_id, id) DO NOTHING",
                &[&owner, &id, &workflow.name],
            )
            .await
            .map_err(|e| storage_err("create", e))?;
        if inserted == 0 {
            return Err(Error::Invalid(format!("workflow déjà existant : {id}")));
        }
        tx.execute(
            "INSERT INTO workflow_versions (owner_id, workflow_id, version, graph) \
             VALUES ($1, $2, 1, $3)",
            &[&owner, &id, &graph],
        )
        .await
        .map_err(|e| storage_err("create version", e))?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    async fn get(
        &self,
        owner: &str,
        id: &WorkflowId,
    ) -> autome_core::Result<Option<WorkflowGraph>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                "SELECT v.graph FROM workflows w \
                 JOIN workflow_versions v \
                   ON v.owner_id = w.owner_id AND v.workflow_id = w.id \
                  AND v.version = w.current_version \
                 WHERE w.owner_id = $1 AND w.id = $2",
                &[&owner, &id.0],
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

    async fn list(&self, owner: &str, paging: Pagination) -> autome_core::Result<WorkflowPage> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let total: i64 = client
            .query_one(
                "SELECT count(*) FROM workflows WHERE owner_id = $1",
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
                   ON v.owner_id = w.owner_id AND v.workflow_id = w.id \
                  AND v.version = w.current_version \
                 WHERE w.owner_id = $1 \
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

    async fn update(&self, owner: &str, workflow: WorkflowGraph) -> autome_core::Result<()> {
        let id = workflow.id.0.clone();
        let graph = serde_json::to_value(&workflow)?;
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // `FOR UPDATE` : deux mises à jour concurrentes du même workflow se
        // sérialisent au lieu de calculer la même version suivante (conflit de
        // clé primaire ⇒ 500).
        let next: Option<i32> = tx
            .query_opt(
                "SELECT current_version + 1 FROM workflows \
                 WHERE owner_id = $1 AND id = $2 FOR UPDATE",
                &[&owner, &id],
            )
            .await
            .map_err(|e| storage_err("update lookup", e))?
            .map(|r| r.get(0));
        let Some(next) = next else {
            return Err(Error::Invalid(format!("workflow inconnu : {id}")));
        };
        tx.execute(
            "INSERT INTO workflow_versions (owner_id, workflow_id, version, graph) \
             VALUES ($1, $2, $3, $4)",
            &[&owner, &id, &next, &graph],
        )
        .await
        .map_err(|e| storage_err("update version", e))?;
        tx.execute(
            "UPDATE workflows SET name = $3, current_version = $4 \
             WHERE owner_id = $1 AND id = $2",
            &[&owner, &id, &workflow.name, &next],
        )
        .await
        .map_err(|e| storage_err("update", e))?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    async fn delete(&self, owner: &str, id: &WorkflowId) -> autome_core::Result<bool> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute(
                "DELETE FROM workflows WHERE owner_id = $1 AND id = $2",
                &[&owner, &id.0],
            )
            .await
            .map_err(|e| storage_err("delete", e))?;
        Ok(n > 0)
    }

    async fn list_all(
        &self,
        filter: &WorkflowFilter,
        paging: Pagination,
    ) -> autome_core::Result<WorkflowListingPage> {
        // `$1` NULL = tous les espaces ; `$2` NULL = pas de recherche (déjà en
        // minuscules). `strpos` plutôt que `LIKE` : aucun caractère joker à
        // échapper dans la saisie.
        const WHERE: &str = "($1::text IS NULL OR w.owner_id = $1) \
             AND ($2::text IS NULL OR strpos(lower(w.id), $2) > 0 \
                  OR strpos(lower(w.name), $2) > 0)";
        let owner = filter.owner.as_deref();
        let needle = filter.needle();
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let total: i64 = client
            .query_one(
                &format!("SELECT count(*) FROM workflows w WHERE {WHERE}"),
                &[&owner, &needle],
            )
            .await
            .map_err(|e| storage_err("list_all count", e))?
            .get(0);
        let limit = paging.limit as i64;
        let offset = paging.offset as i64;
        let rows = client
            .query(
                &format!(
                    "SELECT w.owner_id, w.id, w.name, \
                       COALESCE(jsonb_array_length(v.graph -> 'nodes'), 0) \
                     FROM workflows w \
                     JOIN workflow_versions v \
                       ON v.owner_id = w.owner_id AND v.workflow_id = w.id \
                      AND v.version = w.current_version \
                     WHERE {WHERE} \
                     ORDER BY w.owner_id, w.id LIMIT $3 OFFSET $4"
                ),
                &[&owner, &needle, &limit, &offset],
            )
            .await
            .map_err(|e| storage_err("list_all", e))?;
        let items = rows
            .iter()
            .map(|r| WorkflowListing {
                owner: r.get(0),
                id: WorkflowId::new(r.get::<_, String>(1)),
                name: r.get(2),
                node_count: r.get::<_, i32>(3).max(0) as usize,
            })
            .collect();
        Ok(WorkflowListingPage {
            items,
            total: total as usize,
        })
    }
}

//! [`RunStateStore`] adossé à PostgreSQL (tables `runs` + `run_steps`), **async**.
//!
//! Un `RunCheckpoint` se projette sur une ligne `runs` (scalaires, `next_node`,
//! `error`) et N lignes `run_steps` (la map `outputs`). IDs logiques en TEXT
//! (migration `0009`).

use std::collections::BTreeMap;

use async_trait::async_trait;
use autome_core::{Error, Pagination, RunCheckpoint, RunListPage, RunStateStore, RunStatus};
use serde_json::Value;
use tokio_postgres::Row;

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

fn status_to_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
    }
}

fn status_from_str(s: &str) -> autome_core::Result<RunStatus> {
    match s {
        "queued" => Ok(RunStatus::Queued),
        "running" => Ok(RunStatus::Running),
        "succeeded" => Ok(RunStatus::Succeeded),
        "failed" => Ok(RunStatus::Failed),
        other => Err(Error::Invalid(format!("storage: statut inconnu : {other}"))),
    }
}

/// Store d'état de runs persisté dans PostgreSQL.
#[derive(Clone)]
pub struct PgRunStateStore {
    pool: PgPool,
}

impl PgRunStateStore {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reconstruit un `RunCheckpoint` depuis une ligne `runs` + ses `run_steps`.
    async fn row_to_checkpoint(
        client: &deadpool_postgres::Client,
        run: &Row,
    ) -> autome_core::Result<RunCheckpoint> {
        let run_id: String = run.get("id");
        let steps = client
            .query(
                "SELECT node_id, output FROM run_steps WHERE run_id = $1",
                &[&run_id],
            )
            .await
            .map_err(|e| storage_err("load steps", e))?;
        let outputs: BTreeMap<String, Value> = steps
            .iter()
            .map(|s| (s.get::<_, String>("node_id"), s.get::<_, Value>("output")))
            .collect();
        Ok(RunCheckpoint {
            run_id,
            workflow_id: run.get("workflow_id"),
            version: run.get("version"),
            trigger_type: run.get("trigger_type"),
            status: status_from_str(run.get::<_, &str>("status"))?,
            started_at_ms: run.get("started_at_ms"),
            finished_at_ms: run.get("finished_at_ms"),
            created_at_ms: run.get("created_at_ms"),
            outputs,
            next_node: run.get("next_node"),
            error: run.get("error"),
            owner_id: run.get("owner_id"),
        })
    }
}

const RUN_COLS: &str = "id, workflow_id, version, status, trigger_type, \
                        started_at_ms, finished_at_ms, created_at_ms, next_node, error, owner_id";

#[async_trait]
impl RunStateStore for PgRunStateStore {
    async fn checkpoint(&self, snapshot: &RunCheckpoint) -> autome_core::Result<()> {
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // Upsert de la ligne run (last-write-wins), sauf `owner_id` :
        // immuable une fois posé (COALESCE garde la valeur existante).
        tx.execute(
            "INSERT INTO runs \
             (id, workflow_id, version, status, trigger_type, started_at_ms, \
              finished_at_ms, created_at_ms, next_node, error, owner_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) \
             ON CONFLICT (id) DO UPDATE SET \
               workflow_id = EXCLUDED.workflow_id, version = EXCLUDED.version, \
               status = EXCLUDED.status, trigger_type = EXCLUDED.trigger_type, \
               started_at_ms = EXCLUDED.started_at_ms, \
               finished_at_ms = EXCLUDED.finished_at_ms, \
               created_at_ms = EXCLUDED.created_at_ms, \
               next_node = EXCLUDED.next_node, error = EXCLUDED.error, \
               owner_id = COALESCE(runs.owner_id, EXCLUDED.owner_id)",
            &[
                &snapshot.run_id,
                &snapshot.workflow_id,
                &snapshot.version,
                &status_to_str(snapshot.status),
                &snapshot.trigger_type,
                &snapshot.started_at_ms,
                &snapshot.finished_at_ms,
                &snapshot.created_at_ms,
                &snapshot.next_node,
                &snapshot.error,
                &snapshot.owner_id,
            ],
        )
        .await
        .map_err(|e| storage_err("checkpoint", e))?;

        // Remplace les sorties (la map fait foi).
        tx.execute(
            "DELETE FROM run_steps WHERE run_id = $1",
            &[&snapshot.run_id],
        )
        .await
        .map_err(|e| storage_err("clear steps", e))?;
        for (node_id, output) in &snapshot.outputs {
            tx.execute(
                "INSERT INTO run_steps (run_id, node_id, output) VALUES ($1, $2, $3)",
                &[&snapshot.run_id, node_id, output],
            )
            .await
            .map_err(|e| storage_err("insert step", e))?;
        }
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    async fn load(&self, run_id: &str) -> autome_core::Result<Option<RunCheckpoint>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                &format!("SELECT {RUN_COLS} FROM runs WHERE id = $1"),
                &[&run_id],
            )
            .await
            .map_err(|e| storage_err("load", e))?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(Self::row_to_checkpoint(&client, &r).await?)),
        }
    }

    async fn list_orphans(&self, older_than_ms: i64) -> autome_core::Result<Vec<RunCheckpoint>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let rows = client
            .query(
                &format!(
                    "SELECT {RUN_COLS} FROM runs \
                     WHERE status = 'running' AND started_at_ms IS NOT NULL \
                       AND started_at_ms < $1 ORDER BY id"
                ),
                &[&older_than_ms],
            )
            .await
            .map_err(|e| storage_err("list_orphans", e))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(Self::row_to_checkpoint(&client, r).await?);
        }
        Ok(out)
    }

    async fn mark_failed(
        &self,
        run_id: &str,
        reason: &str,
        now_ms: i64,
    ) -> autome_core::Result<()> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute(
                "UPDATE runs SET status = 'failed', error = $2, finished_at_ms = $3 \
                 WHERE id = $1",
                &[&run_id, &reason, &now_ms],
            )
            .await
            .map_err(|e| storage_err("mark_failed", e))?;
        if n == 0 {
            return Err(Error::Invalid(format!("run inconnu : {run_id}")));
        }
        Ok(())
    }

    async fn list_by_workflow(
        &self,
        workflow_id: &str,
        owner: Option<&str>,
        paging: Pagination,
    ) -> autome_core::Result<RunListPage> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // `$2 IS NULL` ⇒ pas de filtre propriétaire (super-admin).
        let total: i64 = client
            .query_one(
                "SELECT count(*) FROM runs \
                 WHERE workflow_id = $1 AND ($2::text IS NULL OR owner_id = $2)",
                &[&workflow_id, &owner],
            )
            .await
            .map_err(|e| storage_err("list count", e))?
            .get(0);
        let limit = paging.limit as i64;
        let offset = paging.offset as i64;
        let rows = client
            .query(
                &format!(
                    "SELECT {RUN_COLS} FROM runs \
                     WHERE workflow_id = $1 AND ($2::text IS NULL OR owner_id = $2) \
                     ORDER BY created_at_ms DESC, id LIMIT $3 OFFSET $4"
                ),
                &[&workflow_id, &owner, &limit, &offset],
            )
            .await
            .map_err(|e| storage_err("list_by_workflow", e))?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(Self::row_to_checkpoint(&client, r).await?);
        }
        Ok(RunListPage {
            items,
            total: total as usize,
        })
    }
}

//! [`RunStateStore`] adossé à PostgreSQL (tables `runs` + `run_steps`), **async**.
//!
//! Un `RunCheckpoint` se projette sur une ligne `runs` (scalaires, `next_node`,
//! `error`) et N lignes `run_steps` (la map `outputs`). IDs logiques en TEXT
//! (migration `0009`).
//!
//! Nombre d'instructions **indépendant du nombre de nodes et de runs** (#198,
//! AUDIT §10) : un checkpoint écrit toutes ses sorties en un seul upsert
//! (`UNNEST`) et retire les sorties disparues en une suppression ; une liste
//! charge les sorties de tous ses runs en une requête (`run_id = ANY`).

use std::collections::{BTreeMap, HashMap};

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

    /// Reconstruit les `RunCheckpoint` de lignes `runs`, avec les sorties de
    /// **tous** ces runs lues en une seule requête (pas de N+1).
    async fn rows_to_checkpoints(
        client: &deadpool_postgres::Client,
        runs: &[Row],
    ) -> autome_core::Result<Vec<RunCheckpoint>> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = runs.iter().map(|r| r.get("id")).collect();
        let steps = client
            .query(
                "SELECT run_id, node_id, output FROM run_steps WHERE run_id = ANY($1)",
                &[&ids],
            )
            .await
            .map_err(|e| storage_err("load steps", e))?;
        let mut outputs: HashMap<String, BTreeMap<String, Value>> = HashMap::new();
        for step in &steps {
            outputs
                .entry(step.get("run_id"))
                .or_default()
                .insert(step.get("node_id"), step.get("output"));
        }
        runs.iter()
            .map(|run| {
                let id: String = run.get("id");
                let run_outputs = outputs.remove(&id).unwrap_or_default();
                Self::row_to_checkpoint(run, run_outputs)
            })
            .collect()
    }

    /// Un `RunCheckpoint` depuis une ligne `runs` et ses sorties déjà lues.
    fn row_to_checkpoint(
        run: &Row,
        outputs: BTreeMap<String, Value>,
    ) -> autome_core::Result<RunCheckpoint> {
        let run_id: String = run.get("id");
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

        // La map fait foi : toutes les sorties en un upsert, puis suppression
        // des seules sorties disparues — deux instructions quel que soit le
        // nombre de nodes (#198 ; avant : un DELETE puis un INSERT par node).
        let node_ids: Vec<&str> = snapshot.outputs.keys().map(String::as_str).collect();
        let outputs: Vec<&Value> = snapshot.outputs.values().collect();
        if !node_ids.is_empty() {
            tx.execute(
                "INSERT INTO run_steps (run_id, node_id, output) \
                 SELECT $1, node_id, output FROM UNNEST($2::text[], $3::jsonb[]) \
                   AS s(node_id, output) \
                 ON CONFLICT (run_id, node_id) DO UPDATE SET output = EXCLUDED.output",
                &[&snapshot.run_id, &node_ids, &outputs],
            )
            .await
            .map_err(|e| storage_err("upsert steps", e))?;
        }
        tx.execute(
            "DELETE FROM run_steps WHERE run_id = $1 AND node_id <> ALL($2::text[])",
            &[&snapshot.run_id, &node_ids],
        )
        .await
        .map_err(|e| storage_err("prune steps", e))?;
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
            Some(r) => Ok(Self::rows_to_checkpoints(&client, &[r]).await?.pop()),
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
        Self::rows_to_checkpoints(&client, &rows).await
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

    async fn try_claim(
        &self,
        run_id: &str,
        now_ms: i64,
    ) -> autome_core::Result<Option<RunCheckpoint>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Compare-and-set en une instruction : le verrou de ligne de l'UPDATE
        // sérialise les répliques concurrentes ; la seconde réévalue
        // `status = 'queued'` après la première et ne met rien à jour.
        let row = client
            .query_opt(
                &format!(
                    "UPDATE runs SET status = 'running', started_at_ms = $2 \
                     WHERE id = $1 AND status = 'queued' RETURNING {RUN_COLS}"
                ),
                &[&run_id, &now_ms],
            )
            .await
            .map_err(|e| storage_err("try_claim", e))?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Self::rows_to_checkpoints(&client, &[r]).await?.pop()),
        }
    }

    async fn fail_if_running(
        &self,
        run_id: &str,
        reason: &str,
        now_ms: i64,
    ) -> autome_core::Result<bool> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute(
                "UPDATE runs SET status = 'failed', error = $2, finished_at_ms = $3 \
                 WHERE id = $1 AND status = 'running'",
                &[&run_id, &reason, &now_ms],
            )
            .await
            .map_err(|e| storage_err("fail_if_running", e))?;
        Ok(n == 1)
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
        let items = Self::rows_to_checkpoints(&client, &rows).await?;
        Ok(RunListPage {
            items,
            total: total as usize,
        })
    }
}

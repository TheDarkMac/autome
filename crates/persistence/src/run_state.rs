//! [`RunStateStore`] adossé à PostgreSQL (tables `runs`, `run_steps` et
//! `run_outbox`), **async**.
//!
//! Un `RunCheckpoint` se projette sur une ligne `runs` (scalaires, `next_node`,
//! `error`, tentative, battement, point de reprise) et N lignes `run_steps` (la
//! map `outputs`). IDs logiques en TEXT (migration `0009`).
//!
//! **Outbox** (#239, migration `0021`) : le message de file d'un run vit dans
//! `run_outbox`, écrit dans la transaction qui crée le run et retiré par
//! l'écriture de son état final. Les transitions gardées par la tentative
//! (`attempt`) sont des `UPDATE … WHERE attempt = $n` : une seule instruction,
//! le verrou de ligne sérialise les écrivains concurrents.
//!
//! Nombre d'instructions **indépendant du nombre de nodes et de runs** (#198,
//! AUDIT §10) : un checkpoint écrit toutes ses sorties en un seul upsert
//! (`UNNEST`) et retire les sorties disparues en une suppression ; une liste
//! charge les sorties de tous ses runs en une requête (`run_id = ANY`).

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use autome_core::{
    Error, Message, OutboxMessage, Pagination, RunCheckpoint, RunListPage, RunProgress,
    RunStateStore, RunStatus, LEGACY_NAMESPACE,
};
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

/// Sorties d'un checkpoint, dans la transaction `tx`. La map fait foi :
/// toutes les sorties en un upsert, puis suppression des seules sorties
/// disparues — deux instructions quel que soit le nombre de nodes (#198 ;
/// avant : un DELETE puis un INSERT par node).
async fn write_steps(
    tx: &tokio_postgres::Transaction<'_>,
    snapshot: &RunCheckpoint,
) -> autome_core::Result<()> {
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
    Ok(())
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
            // Espace legacy (`''`, migration `0025`) = run sans propriétaire.
            owner_id: Some(run.get::<_, String>("owner_id")).filter(|o| o != LEGACY_NAMESPACE),
            attempt: run.get("attempt"),
            heartbeat_at_ms: run.get("heartbeat_at_ms"),
            resume_state: run.get("resume_state"),
        })
    }
}

const RUN_COLS: &str = "id, workflow_id, version, status, trigger_type, \
                        started_at_ms, finished_at_ms, created_at_ms, next_node, error, owner_id, \
                        attempt, heartbeat_at_ms, resume_state";

/// Valeur de `runs.owner_id` (`NOT NULL`, migration `0025`) : un run sans
/// propriétaire vit dans l'espace legacy, comme les workflows (`0014`).
fn owner_column(snapshot: &RunCheckpoint) -> &str {
    snapshot.owner_id.as_deref().unwrap_or(LEGACY_NAMESPACE)
}

/// Colonnes d'un message de l'outbox, dans l'ordre de [`row_to_outbox`].
const OUTBOX_COLS: &str = "run_id, queue, generation, headers, body";

fn row_to_outbox(row: &Row) -> autome_core::Result<OutboxMessage> {
    let headers: Value = row.get("headers");
    Ok(OutboxMessage {
        run_id: row.get("run_id"),
        queue: row.get("queue"),
        generation: row.get("generation"),
        message: Message {
            headers: serde_json::from_value(headers)
                .map_err(|e| storage_err("outbox headers", e))?,
            body: row.get("body"),
        },
    })
}

#[async_trait]
impl RunStateStore for PgRunStateStore {
    async fn checkpoint(&self, snapshot: &RunCheckpoint) -> autome_core::Result<()> {
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // Upsert de la ligne run (last-write-wins), sauf `owner_id` :
        // immuable, fixé par la première écriture (`''` sans propriétaire).
        tx.execute(
            "INSERT INTO runs \
             (id, workflow_id, version, status, trigger_type, started_at_ms, \
              finished_at_ms, created_at_ms, next_node, error, owner_id, \
              attempt, heartbeat_at_ms, resume_state) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) \
             ON CONFLICT (id) DO UPDATE SET \
               workflow_id = EXCLUDED.workflow_id, version = EXCLUDED.version, \
               status = EXCLUDED.status, trigger_type = EXCLUDED.trigger_type, \
               started_at_ms = EXCLUDED.started_at_ms, \
               finished_at_ms = EXCLUDED.finished_at_ms, \
               created_at_ms = EXCLUDED.created_at_ms, \
               next_node = EXCLUDED.next_node, error = EXCLUDED.error, \
               attempt = EXCLUDED.attempt, heartbeat_at_ms = EXCLUDED.heartbeat_at_ms, \
               resume_state = EXCLUDED.resume_state",
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
                &owner_column(snapshot),
                &snapshot.attempt,
                &snapshot.heartbeat_at_ms,
                &snapshot.resume_state,
            ],
        )
        .await
        .map_err(|e| storage_err("checkpoint", e))?;

        write_steps(&tx, snapshot).await?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    async fn finish(&self, snapshot: &RunCheckpoint) -> autome_core::Result<bool> {
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;
        // `UPDATE` seul, jamais d'insertion : un run effacé entre-temps
        // (#229) n'est pas recréé. Gardé par la tentative : un run repris par
        // un autre worker n'est pas écrasé (#239). Le verrou de ligne
        // sérialise l'écriture avec un effacement concurrent ; `owner_id`
        // reste celui en place.
        let n = tx
            .execute(
                "UPDATE runs SET workflow_id = $2, version = $3, status = $4, \
                   trigger_type = $5, started_at_ms = $6, finished_at_ms = $7, \
                   created_at_ms = $8, next_node = $9, error = $10, \
                   heartbeat_at_ms = $12, resume_state = $13 \
                 WHERE id = $1 AND attempt = $11",
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
                    &snapshot.attempt,
                    &snapshot.heartbeat_at_ms,
                    &snapshot.resume_state,
                ],
            )
            .await
            .map_err(|e| storage_err("finish", e))?;
        if n == 0 {
            return Ok(false);
        }
        write_steps(&tx, snapshot).await?;
        // Run terminé : son message (et le payload qu'il porte) n'a plus lieu
        // d'être.
        tx.execute(
            "DELETE FROM run_outbox WHERE run_id = $1",
            &[&snapshot.run_id],
        )
        .await
        .map_err(|e| storage_err("finish outbox", e))?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(true)
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
        let n: i64 = client
            .query_one(
                "WITH f AS (UPDATE runs SET status = 'failed', error = $2, finished_at_ms = $3 \
                   WHERE id = $1 RETURNING id), \
                 d AS (DELETE FROM run_outbox WHERE run_id IN (SELECT id FROM f)) \
                 SELECT count(*) FROM f",
                &[&run_id, &reason, &now_ms],
            )
            .await
            .map_err(|e| storage_err("mark_failed", e))?
            .get(0);
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
                    "UPDATE runs SET status = 'running', started_at_ms = $2, \
                       heartbeat_at_ms = $2, attempt = attempt + 1 \
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
        let n: i64 = client
            .query_one(
                "WITH f AS (UPDATE runs SET status = 'failed', error = $2, finished_at_ms = $3 \
                   WHERE id = $1 AND status = 'running' RETURNING id), \
                 d AS (DELETE FROM run_outbox WHERE run_id IN (SELECT id FROM f)) \
                 SELECT count(*) FROM f",
                &[&run_id, &reason, &now_ms],
            )
            .await
            .map_err(|e| storage_err("fail_if_running", e))?
            .get(0);
        Ok(n == 1)
    }

    async fn enqueue(
        &self,
        snapshot: &RunCheckpoint,
        message: &OutboxMessage,
    ) -> autome_core::Result<()> {
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;
        // `INSERT` simple : un id déjà pris échoue (clé primaire) au lieu
        // d'écraser un run existant.
        tx.execute(
            "INSERT INTO runs \
             (id, workflow_id, version, status, trigger_type, started_at_ms, \
              finished_at_ms, created_at_ms, next_node, error, owner_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
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
                &owner_column(snapshot),
            ],
        )
        .await
        .map_err(|e| storage_err("enqueue run", e))?;
        let headers = serde_json::to_value(&message.message.headers)?;
        tx.execute(
            "INSERT INTO run_outbox (run_id, queue, generation, headers, body, created_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &message.run_id,
                &message.queue,
                &message.generation,
                &headers,
                &message.message.body,
                &snapshot.created_at_ms,
            ],
        )
        .await
        .map_err(|e| storage_err("enqueue outbox", e))?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(())
    }

    async fn pending_outbox(
        &self,
        created_before_ms: i64,
        limit: usize,
    ) -> autome_core::Result<Vec<OutboxMessage>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = client
            .query(
                &format!(
                    "SELECT {OUTBOX_COLS} FROM run_outbox \
                     WHERE published_at_ms IS NULL AND created_at_ms < $1 \
                     ORDER BY created_at_ms, run_id LIMIT $2"
                ),
                &[&created_before_ms, &limit],
            )
            .await
            .map_err(|e| storage_err("pending_outbox", e))?;
        rows.iter().map(row_to_outbox).collect()
    }

    async fn mark_published(
        &self,
        run_id: &str,
        generation: i32,
        now_ms: i64,
    ) -> autome_core::Result<()> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        client
            .execute(
                "UPDATE run_outbox SET published_at_ms = $3 \
                 WHERE run_id = $1 AND generation = $2",
                &[&run_id, &generation, &now_ms],
            )
            .await
            .map_err(|e| storage_err("mark_published", e))?;
        Ok(())
    }

    async fn record_progress(
        &self,
        run_id: &str,
        attempt: i32,
        progress: &RunProgress,
        now_ms: i64,
    ) -> autome_core::Result<bool> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = match progress {
            RunProgress::Starting { node } => {
                client
                    .execute(
                        "UPDATE runs SET next_node = $3, heartbeat_at_ms = $4 \
                     WHERE id = $1 AND status = 'running' AND attempt = $2",
                        &[&run_id, &attempt, node, &now_ms],
                    )
                    .await
            }
            RunProgress::Done { resume_state } => client
                .execute(
                    "UPDATE runs SET next_node = NULL, resume_state = $3, heartbeat_at_ms = $4 \
                     WHERE id = $1 AND status = 'running' AND attempt = $2",
                    &[&run_id, &attempt, resume_state, &now_ms],
                )
                .await,
        }
        .map_err(|e| storage_err("record_progress", e))?;
        Ok(n == 1)
    }

    async fn heartbeat(
        &self,
        run_id: &str,
        attempt: i32,
        now_ms: i64,
    ) -> autome_core::Result<bool> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute(
                "UPDATE runs SET heartbeat_at_ms = $3 \
                 WHERE id = $1 AND status = 'running' AND attempt = $2",
                &[&run_id, &attempt, &now_ms],
            )
            .await
            .map_err(|e| storage_err("heartbeat", e))?;
        Ok(n == 1)
    }

    async fn list_stale(&self, before_ms: i64) -> autome_core::Result<Vec<RunCheckpoint>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let rows = client
            .query(
                &format!(
                    "SELECT {RUN_COLS} FROM runs \
                     WHERE status = 'running' AND heartbeat_at_ms < $1 ORDER BY id"
                ),
                &[&before_ms],
            )
            .await
            .map_err(|e| storage_err("list_stale", e))?;
        Self::rows_to_checkpoints(&client, &rows).await
    }

    async fn requeue(
        &self,
        run_id: &str,
        attempt: i32,
        now_ms: i64,
    ) -> autome_core::Result<Option<OutboxMessage>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Une instruction : le run repasse `queued` (s'il est encore à cette
        // tentative et a un message) et son message est remis à publier sous
        // la génération suivante. Clé d'idempotence : même format que
        // `autome_core::outbox_idempotency_key` (`<run_id>#<génération>`).
        // Dans `SET`, `o.generation` est l'ancienne valeur.
        let row = client
            .query_opt(
                "WITH r AS (UPDATE runs SET status = 'queued', heartbeat_at_ms = NULL \
                   WHERE id = $1 AND status = 'running' AND attempt = $2 \
                     AND EXISTS (SELECT 1 FROM run_outbox WHERE run_id = $1) \
                   RETURNING id) \
                 UPDATE run_outbox o SET generation = o.generation + 1, \
                   published_at_ms = NULL, created_at_ms = $3, \
                   headers = jsonb_set(o.headers, '{idempotency-key}', \
                     to_jsonb(o.run_id || '#' || (o.generation + 1))) \
                 FROM r WHERE o.run_id = r.id \
                 RETURNING o.run_id, o.queue, o.generation, o.headers, o.body",
                &[&run_id, &attempt, &now_ms],
            )
            .await
            .map_err(|e| storage_err("requeue", e))?;
        row.as_ref().map(row_to_outbox).transpose()
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

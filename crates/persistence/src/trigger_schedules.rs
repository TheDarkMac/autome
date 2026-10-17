//! [`TriggerScheduleStore`] adossé à PostgreSQL (table `trigger_schedules`,
//! #227).
//!
//! `advance` est un `UPDATE … WHERE revision = $n` : sur plusieurs workers,
//! une seule mise à jour aboutit pour une révision donnée (le verrou de ligne
//! sérialise les concurrentes, la seconde ne voit plus la révision attendue).
//! `sync` s'exécute en une transaction ; un nœud inchangé n'est pas réécrit
//! (sa révision, donc un déclenchement en cours, est préservée).
//! Schéma : migration `migrations/0019_trigger_schedules.sql`.

use async_trait::async_trait;
use autome_core::{
    Error, Result, ScheduleSlot, ScheduledTrigger, SlotUpdate, TriggerScheduleStore,
};
use serde_json::Value;

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Échéances des déclencheurs planifiés persistées dans PostgreSQL.
#[derive(Clone)]
pub struct PgTriggerScheduleStore {
    pool: PgPool,
}

impl PgTriggerScheduleStore {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TriggerScheduleStore for PgTriggerScheduleStore {
    async fn sync(
        &self,
        owner: &str,
        workflow_id: &str,
        triggers: &[ScheduledTrigger],
    ) -> Result<()> {
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("trigger_schedules.sync begin", e))?;
        let keep: Vec<&str> = triggers.iter().map(|t| t.node_id.as_str()).collect();
        tx.execute(
            "DELETE FROM trigger_schedules \
             WHERE owner_id = $1 AND workflow_id = $2 AND node_id <> ALL($3)",
            &[&owner, &workflow_id, &keep],
        )
        .await
        .map_err(|e| storage_err("trigger_schedules.sync delete", e))?;
        for t in triggers {
            tx.execute(
                "INSERT INTO trigger_schedules \
                     (owner_id, workflow_id, node_id, trigger_type, params) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (owner_id, workflow_id, node_id) DO UPDATE SET \
                     trigger_type = EXCLUDED.trigger_type, params = EXCLUDED.params, \
                     next_fire_ms = NULL, state = '{}'::jsonb, error = NULL, \
                     revision = trigger_schedules.revision + 1 \
                 WHERE trigger_schedules.trigger_type <> EXCLUDED.trigger_type \
                    OR trigger_schedules.params <> EXCLUDED.params",
                &[&owner, &workflow_id, &t.node_id, &t.trigger_type, &t.params],
            )
            .await
            .map_err(|e| storage_err("trigger_schedules.sync upsert", e))?;
        }
        tx.commit()
            .await
            .map_err(|e| storage_err("trigger_schedules.sync commit", e))
    }

    async fn due(&self, now_ms: i64, limit: usize) -> Result<Vec<ScheduleSlot>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = client
            .query(
                "SELECT owner_id, workflow_id, node_id, trigger_type, params, \
                        next_fire_ms, state, revision \
                 FROM trigger_schedules \
                 WHERE error IS NULL AND (next_fire_ms IS NULL OR next_fire_ms <= $1) \
                 ORDER BY next_fire_ms NULLS FIRST, owner_id, workflow_id, node_id \
                 LIMIT $2",
                &[&now_ms, &limit],
            )
            .await
            .map_err(|e| storage_err("trigger_schedules.due", e))?;
        Ok(rows
            .iter()
            .map(|row| ScheduleSlot {
                owner_id: row.get(0),
                workflow_id: row.get(1),
                node_id: row.get(2),
                trigger_type: row.get(3),
                params: row.get::<_, Value>(4),
                next_fire_ms: row.get(5),
                state: row.get::<_, Value>(6),
                revision: row.get(7),
            })
            .collect())
    }

    async fn advance(&self, slot: &ScheduleSlot, update: SlotUpdate) -> Result<bool> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let updated = client
            .execute(
                "UPDATE trigger_schedules \
                 SET next_fire_ms = $5, state = $6, error = $7, revision = revision + 1 \
                 WHERE owner_id = $1 AND workflow_id = $2 AND node_id = $3 AND revision = $4",
                &[
                    &slot.owner_id,
                    &slot.workflow_id,
                    &slot.node_id,
                    &slot.revision,
                    &update.next_fire_ms,
                    &update.state,
                    &update.error,
                ],
            )
            .await
            .map_err(|e| storage_err("trigger_schedules.advance", e))?;
        Ok(updated == 1)
    }
}

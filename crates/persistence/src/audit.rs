//! [`AuditSink`] adossé à PostgreSQL (table `audit_events`).
//!
//! Append-only, lecture en ordre d'insertion (clé `id` BIGSERIAL). Persiste
//! les events d'audit (F6.6) au-delà du processus. Schéma : migration
//! `migrations/0008_audit_events.sql`.

use autome_core::{AuditEvent, AuditOutcome, AuditSink, Error};
use serde_json::Value;

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Représentation texte stable de l'`outcome` en base.
fn outcome_to_str(o: AuditOutcome) -> &'static str {
    match o {
        AuditOutcome::Success => "success",
        AuditOutcome::Failure => "failure",
        AuditOutcome::Denied => "denied",
    }
}

fn outcome_from_str(s: &str) -> autome_core::Result<AuditOutcome> {
    match s {
        "success" => Ok(AuditOutcome::Success),
        "failure" => Ok(AuditOutcome::Failure),
        "denied" => Ok(AuditOutcome::Denied),
        other => Err(Error::Invalid(format!(
            "storage: outcome inconnu : {other}"
        ))),
    }
}

/// Sink d'audit persistant (PostgreSQL).
#[derive(Clone)]
pub struct PgAuditSink {
    pool: PgPool,
}

impl PgAuditSink {
    /// Construit le sink sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl AuditSink for PgAuditSink {
    fn record(&self, event: AuditEvent) -> autome_core::Result<()> {
        let mut client = self.pool.get().map_err(|e| storage_err("pool", e))?;
        client
            .execute(
                "INSERT INTO audit_events \
                 (timestamp_ms, actor, action, resource, outcome, metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &event.timestamp_ms,
                    &event.actor,
                    &event.action,
                    &event.resource,
                    &outcome_to_str(event.outcome),
                    &event.metadata,
                ],
            )
            .map_err(|e| storage_err("record", e))?;
        Ok(())
    }

    fn list(&self) -> autome_core::Result<Vec<AuditEvent>> {
        let mut client = self.pool.get().map_err(|e| storage_err("pool", e))?;
        let rows = client
            .query(
                "SELECT timestamp_ms, actor, action, resource, outcome, metadata \
                 FROM audit_events ORDER BY id",
                &[],
            )
            .map_err(|e| storage_err("list", e))?;
        rows.iter()
            .map(|r| {
                let outcome = outcome_from_str(r.get::<_, &str>(4))?;
                Ok(AuditEvent {
                    timestamp_ms: r.get(0),
                    actor: r.get(1),
                    action: r.get(2),
                    resource: r.get(3),
                    outcome,
                    metadata: r.get::<_, Value>(5),
                })
            })
            .collect()
    }
}

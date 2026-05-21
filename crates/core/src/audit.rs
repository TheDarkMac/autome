//! Audit log des actions sensibles (F6.6).
//!
//! Enregistre **qui** a fait **quoi**, **sur quelle ressource**, **quand**,
//! et avec quel **résultat** — au minimum les décisions d'autorisation
//! (F6.5), à enrichir progressivement (login, création de credentials,
//! mutations de workflows, …).
//!
//! Types :
//! - [`AuditEvent`] : structure plate sérialisable JSON, exportable vers
//!   un SIEM (Splunk, Elastic, etc.) côté F10 (DevOps).
//! - [`AuditOutcome`] : `Success` / `Failure` / `Denied`. La distinction
//!   `Failure` (erreur technique) vs `Denied` (refus d'autorisation)
//!   évite de noyer un 403 légitime dans le bruit des 500.
//! - [`AuditSink`] : trait append-only ; impl [`InMemoryAuditSink`] pour
//!   tests/dev, impl [`NoopAuditSink`] pour désactiver complètement.
//!
//! Le sink PG (table `audit_log`) viendra avec sqlx. Le contrat suffit
//! pour que les callers (`api::auth`) ne dépendent pas du backend.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// Résultat d'une action auditée.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditOutcome {
    /// L'action s'est déroulée comme prévu.
    Success,
    /// Échec **technique** (lock empoisonné, indispo DB, …). À distinguer
    /// d'un refus d'autorisation.
    Failure,
    /// **Refus d'autorisation** (permission manquante, signature invalide,
    /// quota dépassé). Pas une erreur de l'application.
    Denied,
}

/// Évènement audité. Format **plat** pour faciliter l'export SIEM et le
/// stockage SQL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Horodatage en millisecondes Unix. Fourni par le caller (clock
    /// injectable) — le sink n'appelle pas `SystemTime::now()`.
    pub timestamp_ms: i64,
    /// Identifiant de l'acteur (`claims.sub` typiquement). `"anonymous"`
    /// pour les actions non authentifiées.
    pub actor: String,
    /// Identifiant **stable** de l'action (`"auth.permission_check"`,
    /// `"credentials.create"`, `"workflows.update"`, …). Convention :
    /// `<domain>.<verb>` snake_case.
    pub action: String,
    /// Ressource ciblée (`workflow_id`, `permission`, `credential_id`).
    /// `None` pour les actions sans cible précise.
    pub resource: Option<String>,
    /// Résultat.
    pub outcome: AuditOutcome,
    /// Métadonnées libres : payload JSON arbitraire (claims pertinentes,
    /// IP source, user-agent, raison d'un refus, …). Doit rester petit
    /// — les gros dumps appartiennent à un système séparé.
    #[serde(default)]
    pub metadata: Value,
}

impl AuditEvent {
    /// Builder convenience.
    pub fn new(
        timestamp_ms: i64,
        actor: impl Into<String>,
        action: impl Into<String>,
        outcome: AuditOutcome,
    ) -> Self {
        Self {
            timestamp_ms,
            actor: actor.into(),
            action: action.into(),
            resource: None,
            outcome,
            metadata: Value::Null,
        }
    }

    /// Builder : précise la ressource.
    pub fn with_resource(mut self, resource: impl Into<String>) -> Self {
        self.resource = Some(resource.into());
        self
    }

    /// Builder : attache des métadonnées.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Sink append-only pour les events audités.
///
/// Volontairement sync (les flushs réseau du sink réel se gèrent dans
/// l'impl via une queue interne).
pub trait AuditSink {
    /// Enregistre `event`. Ne doit jamais paniquer ni bloquer
    /// indéfiniment côté impl réelle.
    fn record(&self, event: AuditEvent) -> Result<()>;

    /// Liste tous les events enregistrés (ordre FIFO d'insertion).
    /// Le filtrage et la pagination sont à la charge du caller — ce trait
    /// reste minimal.
    fn list(&self) -> Result<Vec<AuditEvent>>;
}

// ────────────────────── InMemoryAuditSink ──────────────────────

/// Impl in-process, `Mutex<Vec<AuditEvent>>`. Suffisant pour tests et
/// mode dev. Pour la prod, brancher un sink PG/Kafka/SIEM via le trait.
#[derive(Debug, Default)]
pub struct InMemoryAuditSink {
    events: Mutex<Vec<AuditEvent>>,
}

impl InMemoryAuditSink {
    /// Crée un sink vide.
    pub fn new() -> Self {
        Self::default()
    }

    /// Nombre d'events enregistrés (sans clone).
    pub fn len(&self) -> usize {
        self.events.lock().map(|v| v.len()).unwrap_or(0)
    }

    /// `true` si aucun event n'a été enregistré.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AuditSink for InMemoryAuditSink {
    fn record(&self, event: AuditEvent) -> Result<()> {
        self.events
            .lock()
            .map_err(|_| Error::Invalid("audit lock empoisonné".into()))?
            .push(event);
        Ok(())
    }

    fn list(&self) -> Result<Vec<AuditEvent>> {
        Ok(self
            .events
            .lock()
            .map_err(|_| Error::Invalid("audit lock empoisonné".into()))?
            .clone())
    }
}

// ────────────────────── NoopAuditSink ──────────────────────

/// Sink no-op : drop tous les events. Utile pour désactiver l'audit
/// (déploiement embarqué, tests qui ne se soucient pas de l'audit).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuditSink;

impl AuditSink for NoopAuditSink {
    fn record(&self, _event: AuditEvent) -> Result<()> {
        Ok(())
    }

    fn list(&self) -> Result<Vec<AuditEvent>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ───── AuditEvent / AuditOutcome ─────

    #[test]
    fn event_builder_round_trips_fields() {
        let e = AuditEvent::new(
            1234,
            "alice",
            "auth.permission_check",
            AuditOutcome::Success,
        )
        .with_resource("users.admin")
        .with_metadata(json!({ "from": "127.0.0.1" }));
        assert_eq!(e.timestamp_ms, 1234);
        assert_eq!(e.actor, "alice");
        assert_eq!(e.action, "auth.permission_check");
        assert_eq!(e.outcome, AuditOutcome::Success);
        assert_eq!(e.resource.as_deref(), Some("users.admin"));
        assert_eq!(e.metadata, json!({ "from": "127.0.0.1" }));
    }

    #[test]
    fn event_serializes_to_json_with_snake_case_outcome() {
        let e = AuditEvent::new(0, "bob", "x.y", AuditOutcome::Denied);
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"outcome\":\"denied\""), "json={s}");
        let back: AuditEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn outcome_variants_are_serialized_distinctly() {
        for (o, expected) in [
            (AuditOutcome::Success, "\"success\""),
            (AuditOutcome::Failure, "\"failure\""),
            (AuditOutcome::Denied, "\"denied\""),
        ] {
            assert_eq!(serde_json::to_string(&o).unwrap(), expected);
        }
    }

    // ───── InMemoryAuditSink ─────

    #[test]
    fn fresh_sink_is_empty() {
        let s = InMemoryAuditSink::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn record_then_list_preserves_fifo_order() {
        let s = InMemoryAuditSink::new();
        for i in 0..3 {
            s.record(AuditEvent::new(
                i * 1000,
                format!("user-{i}"),
                "x.y",
                AuditOutcome::Success,
            ))
            .unwrap();
        }
        let events = s.list().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].timestamp_ms, 0);
        assert_eq!(events[1].timestamp_ms, 1000);
        assert_eq!(events[2].timestamp_ms, 2000);
        assert_eq!(events[0].actor, "user-0");
        assert_eq!(events[2].actor, "user-2");
    }

    #[test]
    fn list_returns_a_clone_not_a_view() {
        let s = InMemoryAuditSink::new();
        s.record(AuditEvent::new(1, "a", "x", AuditOutcome::Success))
            .unwrap();
        let snapshot = s.list().unwrap();
        s.record(AuditEvent::new(2, "b", "x", AuditOutcome::Success))
            .unwrap();
        // Le `snapshot` n'a pas changé après le 2ᵉ record
        assert_eq!(snapshot.len(), 1);
        assert_eq!(s.list().unwrap().len(), 2);
    }

    #[test]
    fn sink_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryAuditSink>();
        assert_send_sync::<NoopAuditSink>();
    }

    // ───── NoopAuditSink ─────

    #[test]
    fn noop_drops_events_silently() {
        let s = NoopAuditSink;
        s.record(AuditEvent::new(1, "a", "x", AuditOutcome::Success))
            .unwrap();
        s.record(AuditEvent::new(2, "b", "y", AuditOutcome::Denied))
            .unwrap();
        assert!(s.list().unwrap().is_empty());
    }
}

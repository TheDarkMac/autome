//! `autome-core` — socle partagé du backend Autome.
//!
//! Ce crate ne dépend d'aucun autre crate interne (cf. ARCHITECTURE.md §9) :
//! il expose les types, erreurs et traits réutilisés par `engine`,
//! `connectors`, `api` et `worker`. Squelette posé en F1.3.b ; les types
//! métier (graphe de workflow, runs) sont détaillés en F1.4.

#![doc(html_no_source)]

pub mod action;
pub mod audit;
pub mod auth;
pub mod config;
pub mod queue;
pub mod rbac;
pub mod run_state;
pub mod scheduler;
pub mod secrets;
pub mod trigger;
pub mod workflow;

pub use action::Action;
pub use audit::{AuditEvent, AuditOutcome, AuditSink, InMemoryAuditSink, NoopAuditSink};
pub use auth::{
    Argon2PasswordHasher, Claims, InMemoryRevocationStore, PasswordHasher, RevocationStore,
    TokenKind, TokenService,
};
pub use config::{Config, Profile, Secret};
pub use queue::{dlq_of, BoundedQueue, Delivery, DeliveryId, InMemoryQueue, Message, Nack, Queue};
pub use rbac::{
    has_permission, InMemoryPermissionStore, Permission, PermissionStore, Role,
    PERM_CREDENTIALS_MANAGE, PERM_RUNS_EXECUTE, PERM_RUNS_READ, PERM_USERS_ADMIN, PERM_WILDCARD,
    PERM_WORKFLOWS_READ, PERM_WORKFLOWS_WRITE,
};
pub use run_state::{InMemoryRunStateStore, RunCheckpoint, RunStateStore, RunStatus};
pub use scheduler::{
    default_dedup_key, DedupKeyFn, DedupLock, InMemoryDedupLock, RunningScheduler, Scheduler,
};
pub use secrets::{EnvelopeCipher, InMemoryKeyProvider, KeyProvider};
pub use trigger::{Trigger, TriggerEvent};
pub use workflow::repo::{
    InMemoryWorkflowRepository, Pagination, WorkflowPage, WorkflowRepository,
};
pub use workflow::{Edge, Node, NodeKind, WorkflowGraph, WorkflowId};

use thiserror::Error;

/// Alias de résultat standard du backend.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Erreur racine partagée par tous les crates.
///
/// Chaque couche enrichira ses variantes (moteur, connecteurs, API) ;
/// le découpage fin relève des features dédiées (F2.7 gestion d'erreurs).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Donnée d'entrée invalide (validation de graphe, paramètres, etc.).
    #[error("entrée invalide : {0}")]
    Invalid(String),

    /// Échec de (dé)sérialisation d'un modèle workflow / payload.
    #[error("erreur de sérialisation : {0}")]
    Serialization(#[from] serde_json::Error),

    /// File saturée — un producteur tente de publier sur une queue qui a
    /// atteint sa limite (F5.5). Le caller est invité à ralentir ou retenter
    /// plus tard, **pas** à transformer ça en `Failed` côté run.
    #[error("backpressure : queue '{queue}' pleine ({depth}/{max})")]
    Backpressure {
        /// Nom de la queue saturée.
        queue: String,
        /// Profondeur observée.
        depth: usize,
        /// Limite configurée.
        max: usize,
    },
}

/// Identifiant logique d'un node dans un graphe de workflow.
///
/// Type-newtype volontairement minimal ; la structure complète du graphe
/// (nodes, edges, params) est posée en F1.4 / F2.1.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// Construit un identifiant de node à partir de n'importe quel libellé.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_roundtrips_through_json() {
        let id = NodeId::new("trigger-1");
        let json = serde_json::to_string(&id).expect("serialize");
        let back: NodeId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn serde_error_maps_into_core_error() {
        let err: Error = serde_json::from_str::<NodeId>("not json")
            .map(|_| ())
            .unwrap_err()
            .into();
        assert!(matches!(err, Error::Serialization(_)));
    }
}

//! `autome-core` — socle partagé du backend Autome.
//!
//! Ce crate ne dépend d'aucun autre crate interne (cf. ARCHITECTURE.md §9) :
//! il expose les types, erreurs et traits réutilisés par `engine`,
//! `connectors`, `api` et `worker`. Squelette posé en F1.3.b ; les types
//! métier (graphe de workflow, runs) sont détaillés en F1.4.

#![doc(html_no_source)]

pub mod action;
pub mod config;
pub mod trigger;
pub mod workflow;

pub use action::Action;
pub use config::{Config, Profile, Secret};
pub use trigger::{Trigger, TriggerEvent};
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

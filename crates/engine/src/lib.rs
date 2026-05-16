//! `autome-engine` — moteur d'exécution des workflows (DAG).
//!
//! Dépend de `core` et `connectors` (ARCHITECTURE.md §9), jamais de `api`
//! ni d'un SGBD : le cœur reste agnostique. Le graphe, le tri topologique,
//! les branches/boucles/parallélisme et la gestion d'erreurs relèvent du
//! domaine 2 (F2.*). Ici : squelette F1.3.c.

use autome_core::{NodeId, Result};

/// État d'exécution d'un run, vue minimale.
///
/// Le modèle complet (RUNS / RUN_STEPS) est posé en F1.5 ; cet enum sert de
/// point d'ancrage typé pour les premières itérations du moteur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// En file, pas encore pris par un worker.
    Queued,
    /// En cours d'exécution.
    Running,
    /// Terminé avec succès.
    Succeeded,
    /// Terminé en échec.
    Failed,
}

/// Moteur d'exécution (placeholder).
///
/// L'exécution réelle du graphe arrive en F2.2 ; on expose pour l'instant
/// une surface stable pour que `worker` puisse dépendre du moteur.
#[derive(Debug, Default)]
pub struct Engine;

impl Engine {
    /// Crée un moteur.
    pub fn new() -> Self {
        Self
    }

    /// Point d'entrée d'exécution d'un node (non implémenté — F2.2).
    ///
    /// Retourne une erreur explicite tant que le moteur n'est pas câblé,
    /// pour éviter tout faux positif silencieux.
    pub fn execute_node(&self, node: &NodeId) -> Result<RunStatus> {
        Err(autome_core::Error::Invalid(format!(
            "exécution du node `{}` non implémentée (F2.2)",
            node.0
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_node_is_not_yet_implemented() {
        let engine = Engine::new();
        let res = engine.execute_node(&NodeId::new("n1"));
        assert!(res.is_err());
    }

    #[test]
    fn run_status_variants_are_distinct() {
        assert_ne!(RunStatus::Queued, RunStatus::Running);
        assert_ne!(RunStatus::Succeeded, RunStatus::Failed);
    }
}

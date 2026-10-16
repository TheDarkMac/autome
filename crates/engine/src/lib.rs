//! `autome-engine` — moteur d'exécution des workflows (DAG).
//!
//! Dépend de `core` et `connectors` (ARCHITECTURE.md §9), jamais de `api`
//! ni d'un SGBD : le cœur reste agnostique. Le graphe validé est fourni
//! par [`graph`] (F2.1) ; l'exécution séquentielle par [`exec`] (F2.2).
//! Branches/boucles/parallélisme et retry/timeout relèvent de F2.3–F2.7.

pub mod condition;
pub mod exec;
pub mod graph;
pub mod mapping;

pub use exec::{
    run_parallel, run_sequential, NodeRunner, RunContext, RunReport, StepOutcome, StepStatus,
};
pub use graph::{Dag, GraphError};
pub use mapping::{eval_expression, render_template, resolve_params};

// `RunStatus` est désormais défini dans `autome_core::run_state` (F5.4) —
// type partagé entre l'engine et le store de persistance d'état. Le
// re-export ci-dessous garde la rétrocompatibilité (`use autome_engine::RunStatus`).
pub use autome_core::RunStatus;

/// Moteur d'exécution de workflows.
#[derive(Debug, Default)]
pub struct Engine;

impl Engine {
    /// Crée un moteur.
    pub fn new() -> Self {
        Self
    }

    /// Exécute un DAG validé séquentiellement via le `runner` fourni.
    pub fn run(&self, dag: &Dag, runner: &(dyn NodeRunner + Sync)) -> RunReport {
        run_sequential(dag, runner)
    }

    /// Exécute un DAG validé en parallèle (fan-out/fan-in), avec au plus
    /// `max_concurrency` nodes exécutés simultanément (F2.5).
    pub fn run_parallel(
        &self,
        dag: &Dag,
        runner: &(dyn NodeRunner + Sync),
        max_concurrency: usize,
    ) -> RunReport {
        run_parallel(dag, runner, max_concurrency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{Node, NodeId, NodeKind, WorkflowGraph, WorkflowId};

    #[test]
    fn engine_run_executes_single_node_workflow() {
        let dag = Dag::from_workflow(&WorkflowGraph {
            id: WorkflowId::new("wf"),
            name: "t".into(),
            nodes: vec![Node {
                position: None,
                id: NodeId::new("t"),
                name: "trigger".into(),
                kind: NodeKind::Trigger {
                    trigger_type: "manual".into(),
                },
                params: serde_json::Value::Null,
            }],
            edges: vec![],
        })
        .expect("DAG valide");

        let runner = |_: &Node, _: &RunContext| Ok(serde_json::Value::Null);
        let report = Engine::new().run(&dag, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(report.steps.len(), 1);
    }

    #[test]
    fn run_status_variants_are_distinct() {
        assert_ne!(RunStatus::Queued, RunStatus::Running);
        assert_ne!(RunStatus::Succeeded, RunStatus::Failed);
    }
}

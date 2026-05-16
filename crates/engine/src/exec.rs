//! Exécution séquentielle d'un workflow validé (F2.2).
//!
//! Parcourt le [`Dag`] (F2.1) dans l'ordre topologique (F2.2.a), exécute
//! chaque node via un [`NodeRunner`] injecté, accumule les sorties dans un
//! [`RunContext`] (F2.2.b) et produit un [`RunReport`] par node (F2.2.c).
//!
//! Le moteur reste **agnostique** : il ne sait pas *comment* un node
//! s'exécute (HTTP, Slack, …) — c'est le `NodeRunner` qui le décide. Les
//! implémentations concrètes relèvent des domaines 3/4.
//!
//! **Hors périmètre F2.2** : branches conditionnelles (F2.3), mapping de
//! données entre nodes (F2.6), retry/timeout (F2.7). Ici : exécution
//! linéaire **fail-fast** (au premier échec, les nodes restants sont
//! marqués `Skipped`).

use std::collections::HashMap;

use autome_core::{Node, NodeId};
use serde_json::Value;

use crate::{Dag, RunStatus};

/// Statut d'un node exécuté dans un run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// Exécuté avec succès.
    Succeeded,
    /// A échoué (voir `error`).
    Failed,
    /// Non exécuté car un node précédent a échoué (fail-fast).
    Skipped,
}

/// Résultat d'exécution d'un node.
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    /// Node concerné.
    pub node_id: NodeId,
    /// Statut final du node.
    pub status: StepStatus,
    /// Sortie produite (présente seulement si `Succeeded`).
    pub output: Option<Value>,
    /// Message d'erreur (présent seulement si `Failed`).
    pub error: Option<String>,
}

/// Contexte d'un run : sorties accumulées des nodes déjà exécutés (F2.2.b).
///
/// Un `NodeRunner` peut y lire la sortie d'un prédécesseur. Le *mapping*
/// fin des champs d'entrée relève de F2.6.
#[derive(Debug, Default)]
pub struct RunContext {
    outputs: HashMap<NodeId, Value>,
}

impl RunContext {
    /// Sortie d'un node déjà exécuté, si disponible.
    pub fn output(&self, id: &NodeId) -> Option<&Value> {
        self.outputs.get(id)
    }

    fn record(&mut self, id: NodeId, value: Value) {
        self.outputs.insert(id, value);
    }
}

/// Stratégie d'exécution d'un node, injectée dans le moteur.
///
/// `Err(message)` => le node est en échec ; le run s'arrête (fail-fast).
pub trait NodeRunner {
    /// Exécute `node`, avec accès en lecture aux sorties précédentes.
    fn run(&self, node: &Node, ctx: &RunContext) -> Result<Value, String>;
}

/// Implémentation de [`NodeRunner`] à partir d'une closure (tests, glue).
impl<F> NodeRunner for F
where
    F: Fn(&Node, &RunContext) -> Result<Value, String>,
{
    fn run(&self, node: &Node, ctx: &RunContext) -> Result<Value, String> {
        self(node, ctx)
    }
}

/// Rapport d'un run : statut global + résultat par node (F2.2.c).
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    /// Statut global du run.
    pub status: RunStatus,
    /// Résultats, dans l'ordre topologique d'exécution.
    pub steps: Vec<StepOutcome>,
}

impl RunReport {
    /// Résultat d'un node donné, si présent dans le run.
    pub fn step(&self, id: &NodeId) -> Option<&StepOutcome> {
        self.steps.iter().find(|s| &s.node_id == id)
    }
}

/// Exécute le DAG séquentiellement (ordre topologique), fail-fast.
pub fn run_sequential(dag: &Dag, runner: &dyn NodeRunner) -> RunReport {
    let order = dag.topological_order();
    let mut ctx = RunContext::default();
    let mut steps = Vec::with_capacity(order.len());
    let mut failed = false;

    for id in &order {
        if failed {
            steps.push(StepOutcome {
                node_id: id.clone(),
                status: StepStatus::Skipped,
                output: None,
                error: None,
            });
            continue;
        }
        let node = dag.node(id).expect("node issu du DAG");
        match runner.run(node, &ctx) {
            Ok(out) => {
                ctx.record(id.clone(), out.clone());
                steps.push(StepOutcome {
                    node_id: id.clone(),
                    status: StepStatus::Succeeded,
                    output: Some(out),
                    error: None,
                });
            }
            Err(e) => {
                failed = true;
                steps.push(StepOutcome {
                    node_id: id.clone(),
                    status: StepStatus::Failed,
                    output: None,
                    error: Some(e),
                });
            }
        }
    }

    let status = if failed {
        RunStatus::Failed
    } else {
        RunStatus::Succeeded
    };
    RunReport { status, steps }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{Edge, NodeKind, WorkflowGraph, WorkflowId};

    fn n(id: &str) -> Node {
        Node {
            id: NodeId::new(id),
            name: id.into(),
            kind: NodeKind::Action {
                action_type: "noop".into(),
            },
            params: Value::Null,
        }
    }
    fn trig(id: &str) -> Node {
        Node {
            id: NodeId::new(id),
            name: id.into(),
            kind: NodeKind::Trigger {
                trigger_type: "manual".into(),
            },
            params: Value::Null,
        }
    }
    fn e(from: &str, to: &str) -> Edge {
        Edge {
            from: NodeId::new(from),
            to: NodeId::new(to),
            from_port: None,
        }
    }
    fn dag(nodes: Vec<Node>, edges: Vec<Edge>) -> Dag {
        Dag::from_workflow(&WorkflowGraph {
            id: WorkflowId::new("wf"),
            name: "t".into(),
            nodes,
            edges,
        })
        .expect("DAG valide")
    }

    #[test]
    fn runs_all_nodes_in_topological_order_on_success() {
        let d = dag(
            vec![trig("t"), n("a"), n("b")],
            vec![e("t", "a"), e("a", "b")],
        );
        let runner = |node: &Node, _: &RunContext| Ok(serde_json::json!({ "id": node.id.0 }));
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        let seq: Vec<String> = report.steps.iter().map(|s| s.node_id.0.clone()).collect();
        assert_eq!(seq, vec!["t", "a", "b"]);
        assert!(report
            .steps
            .iter()
            .all(|s| s.status == StepStatus::Succeeded));
    }

    #[test]
    fn context_exposes_predecessor_outputs() {
        let d = dag(vec![trig("t"), n("a")], vec![e("t", "a")]);
        let runner = |node: &Node, ctx: &RunContext| {
            if node.id.0 == "a" {
                // La sortie du trigger doit être visible.
                let upstream = ctx.output(&NodeId::new("t")).expect("sortie de t");
                assert_eq!(upstream, &serde_json::json!("hello"));
            }
            Ok(serde_json::json!("hello"))
        };
        let report = run_sequential(&d, &runner);
        assert_eq!(report.status, RunStatus::Succeeded);
    }

    #[test]
    fn fail_fast_marks_downstream_skipped() {
        let d = dag(
            vec![trig("t"), n("a"), n("b")],
            vec![e("t", "a"), e("a", "b")],
        );
        let runner = |node: &Node, _: &RunContext| {
            if node.id.0 == "a" {
                Err("boom".to_string())
            } else {
                Ok(Value::Null)
            }
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(
            report.step(&NodeId::new("t")).unwrap().status,
            StepStatus::Succeeded
        );
        let a = report.step(&NodeId::new("a")).unwrap();
        assert_eq!(a.status, StepStatus::Failed);
        assert_eq!(a.error.as_deref(), Some("boom"));
        assert_eq!(
            report.step(&NodeId::new("b")).unwrap().status,
            StepStatus::Skipped
        );
    }
}

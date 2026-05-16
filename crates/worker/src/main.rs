//! `autome-worker` — exécuteur de runs.
//!
//! Stateless (ARCHITECTURE.md §2) : consomme un message `{run_id}` depuis la
//! file, charge l'état en base, délègue l'exécution à `engine`. La file
//! (F5.1), le pool de workers (F5.2) et la persistance (F5.4) arrivent au
//! domaine 5. Ici : câblage minimal du moteur séquentiel (F2.2).

use autome_core::{Config, Node, NodeId, NodeKind, WorkflowGraph, WorkflowId};
use autome_engine::{Dag, Engine, RunContext, RunStatus};
use serde_json::Value;

/// Exécute un run de démonstration et renvoie son statut global.
///
/// Le chargement réel du workflow depuis la base et le runner concret
/// (connecteurs) relèvent de F5.*/F3.*/F4. : ici un DAG mono-node et un
/// runner no-op suffisent à exercer le moteur de bout en bout.
fn process_run(run_id: &str) -> autome_core::Result<RunStatus> {
    let workflow = WorkflowGraph {
        id: WorkflowId::new(run_id),
        name: format!("run::{run_id}"),
        nodes: vec![Node {
            id: NodeId::new("entry"),
            name: "entry".into(),
            kind: NodeKind::Trigger {
                trigger_type: "manual".into(),
            },
            params: Value::Null,
        }],
        edges: vec![],
    };
    let dag =
        Dag::from_workflow(&workflow).map_err(|e| autome_core::Error::Invalid(e.to_string()))?;

    let runner = |_: &Node, _: &RunContext| Ok(Value::Null);
    let report = Engine::new().run(&dag, &runner);
    Ok(report.status)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    println!(
        "autome-worker ({:?}) démarré (en attente d'une file — F5.1)",
        config.profile
    );
    let status = process_run("bootstrap")?;
    println!("run bootstrap terminé : {status:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_run_succeeds_on_minimal_workflow() {
        assert_eq!(process_run("run-1").unwrap(), RunStatus::Succeeded);
    }
}

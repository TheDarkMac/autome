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
//! Branch-aware (F2.3) : `If`/`Switch` sont évalués par le moteur et
//! n'activent que la branche retenue. `Loop`/`While` (F2.4) sont aussi
//! évalués par le moteur : ils délèguent le corps au runner par itération
//! (élément/index exposés au contexte) et agrègent les sorties en tableau.
//! Fail-fast : au premier échec, le reste est `Skipped`.
//!
//! **Hors périmètre** : mapping/templating de données (F2.6),
//! parallélisme (F2.5), retry/timeout (F2.7).

use std::collections::{HashMap, HashSet};

use autome_core::{Node, NodeId, NodeKind};
use serde_json::Value;

use crate::{condition, Dag, RunStatus};

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

    fn forget(&mut self, id: &NodeId) {
        self.outputs.remove(id);
    }

    /// Injection directe d'une sortie (tests uniquement).
    #[cfg(test)]
    pub(crate) fn record_for_test(&mut self, id: NodeId, value: Value) {
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

/// Quelles arêtes sortantes d'un node propagent l'activation.
enum Outflow {
    /// Toutes les arêtes (sortie par défaut : trigger/action/loop/while).
    All,
    /// Seules les arêtes dont `from_port == Some(port)` (if/switch).
    Port(String),
    /// Aucune arête (switch sans branche correspondante ni défaut).
    None,
}

/// Issue de l'exécution d'un node : sa sortie et la propagation choisie.
type NodeResult = Result<(Value, Outflow), String>;

/// Évalue un node `If` : `params.condition` (chaîne) → port `true`/`false`.
fn eval_if(node: &Node, ctx: &RunContext) -> NodeResult {
    let cond = node
        .params
        .get("condition")
        .and_then(Value::as_str)
        .ok_or_else(|| "node `if` : `params.condition` (chaîne) requis".to_string())?;
    let taken = condition::eval_bool(cond, ctx)?;
    let port = if taken { "true" } else { "false" };
    Ok((Value::Bool(taken), Outflow::Port(port.to_string())))
}

/// Évalue un node `Switch` : `params.selector` comparé à `params.cases`
/// (`[{ "value": <json>, "port": "<branche>" }]`), `params.default`
/// optionnel. Sortie = valeur du sélecteur ; propagation = port choisi.
fn eval_switch(node: &Node, ctx: &RunContext) -> NodeResult {
    let selector = node
        .params
        .get("selector")
        .and_then(Value::as_str)
        .ok_or_else(|| "node `switch` : `params.selector` (chaîne) requis".to_string())?;
    let value = condition::resolve(selector, ctx)?;

    let cases = node
        .params
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| "node `switch` : `params.cases` (tableau) requis".to_string())?;

    for case in cases {
        let cv = case
            .get("value")
            .ok_or_else(|| "case `switch` sans champ `value`".to_string())?;
        let port = case
            .get("port")
            .and_then(Value::as_str)
            .ok_or_else(|| "case `switch` sans `port` (chaîne)".to_string())?;
        if condition::json_eq(&value, cv) {
            return Ok((value, Outflow::Port(port.to_string())));
        }
    }
    match node.params.get("default").and_then(Value::as_str) {
        Some(p) => Ok((value, Outflow::Port(p.to_string()))),
        None => Ok((value, Outflow::None)),
    }
}

/// Clé de contexte exposant l'élément courant d'une boucle `Loop`
/// (`‹loop_id›::item`) — lisible par le runner / les conditions.
fn item_key(node_id: &str) -> NodeId {
    NodeId::new(format!("{node_id}::item"))
}

/// Clé de contexte exposant l'index d'itération courant (`‹id›::index`).
fn index_key(node_id: &str) -> NodeId {
    NodeId::new(format!("{node_id}::index"))
}

/// Évalue un node `Loop` : itère sur la collection `params.items`
/// (chemin/opérande résolu en tableau). À chaque itération, l'élément et
/// l'index sont exposés au contexte (`‹id›::item` / `::index`) et le
/// **corps** est délégué au `runner`. Sortie = tableau des sorties par
/// itération (F2.4.c). Fail-fast : une itération en échec stoppe la boucle.
fn eval_loop(node: &Node, ctx: &mut RunContext, runner: &dyn NodeRunner) -> NodeResult {
    let expr = node
        .params
        .get("items")
        .and_then(Value::as_str)
        .ok_or_else(|| "node `loop` : `params.items` (chaîne) requis".to_string())?;
    let items = match condition::resolve(expr, ctx)? {
        Value::Array(a) => a,
        other => {
            return Err(format!(
                "node `loop` : `items` doit être un tableau (reçu : {other})"
            ))
        }
    };

    let id = node.id.0.clone();
    let (ik, xk) = (item_key(&id), index_key(&id));
    let mut results = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        ctx.record(ik.clone(), item);
        ctx.record(xk.clone(), Value::from(i as i64));
        let outcome = runner.run(node, ctx);
        match outcome {
            Ok(out) => results.push(out),
            Err(e) => {
                ctx.forget(&ik);
                ctx.forget(&xk);
                return Err(format!("itération {i} : {e}"));
            }
        }
    }
    ctx.forget(&ik);
    ctx.forget(&xk);
    Ok((Value::Array(results), Outflow::All))
}

/// Borne d'itérations par défaut d'un `While` (garde anti-boucle infinie).
const DEFAULT_MAX_ITERATIONS: u64 = 1000;

/// Évalue un node `While` : tant que `params.condition` est vraie, délègue
/// le corps au `runner`. `params.max_iterations` (défaut
/// [`DEFAULT_MAX_ITERATIONS`]) borne l'exécution : si la condition reste
/// vraie au-delà, c'est une **erreur** (garde anti-boucle infinie, F2.4.b).
/// L'index d'itération est exposé (`‹id›::index`). Sortie = tableau des
/// sorties par itération (F2.4.c).
fn eval_while(node: &Node, ctx: &mut RunContext, runner: &dyn NodeRunner) -> NodeResult {
    let cond = node
        .params
        .get("condition")
        .and_then(Value::as_str)
        .ok_or_else(|| "node `while` : `params.condition` (chaîne) requis".to_string())?
        .to_string();
    let max = node
        .params
        .get("max_iterations")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_ITERATIONS);

    let id = node.id.0.clone();
    let xk = index_key(&id);
    let mut results = Vec::new();
    let mut i: u64 = 0;
    loop {
        ctx.record(xk.clone(), Value::from(i as i64));
        let keep_going = match condition::eval_bool(&cond, ctx) {
            Ok(b) => b,
            Err(e) => {
                ctx.forget(&xk);
                return Err(e);
            }
        };
        if !keep_going {
            break;
        }
        if i >= max {
            ctx.forget(&xk);
            return Err(format!(
                "garde anti-boucle : `max_iterations` ({max}) atteint, condition encore vraie"
            ));
        }
        match runner.run(node, ctx) {
            Ok(out) => results.push(out),
            Err(e) => {
                ctx.forget(&xk);
                return Err(format!("itération {i} : {e}"));
            }
        }
        i += 1;
    }
    ctx.forget(&xk);
    Ok((Value::Array(results), Outflow::All))
}

/// Exécute le DAG séquentiellement dans l'ordre topologique.
///
/// Branch-aware (F2.3) : `If`/`Switch` sont évalués par le moteur (pas le
/// runner) et n'activent que les nodes du port retenu ; les nodes jamais
/// activés sont `Skipped`. Fail-fast : au premier échec, le reste est
/// `Skipped` et le run est `Failed`.
pub fn run_sequential(dag: &Dag, runner: &dyn NodeRunner) -> RunReport {
    let order = dag.topological_order();
    let mut ctx = RunContext::default();
    let mut steps = Vec::with_capacity(order.len());
    let mut active: HashSet<NodeId> = HashSet::new();
    active.insert(dag.root().id.clone());
    let mut failed = false;

    for id in &order {
        let skip = |steps: &mut Vec<StepOutcome>| {
            steps.push(StepOutcome {
                node_id: id.clone(),
                status: StepStatus::Skipped,
                output: None,
                error: None,
            });
        };
        if failed || !active.contains(id) {
            skip(&mut steps);
            continue;
        }

        let node = dag.node(id).expect("node issu du DAG");
        let result: NodeResult = match &node.kind {
            NodeKind::If => eval_if(node, &ctx),
            NodeKind::Switch => eval_switch(node, &ctx),
            NodeKind::Loop => eval_loop(node, &mut ctx, runner),
            NodeKind::While => eval_while(node, &mut ctx, runner),
            _ => runner.run(node, &ctx).map(|out| (out, Outflow::All)),
        };

        match result {
            Ok((out, outflow)) => {
                ctx.record(id.clone(), out.clone());
                for e in dag.successors(id) {
                    let take = match &outflow {
                        Outflow::All => true,
                        Outflow::Port(p) => e.from_port.as_deref() == Some(p.as_str()),
                        Outflow::None => false,
                    };
                    if take {
                        active.insert(e.to.clone());
                    }
                }
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

    fn ctrl(id: &str, kind: NodeKind, params: Value) -> Node {
        Node {
            id: NodeId::new(id),
            name: id.into(),
            kind,
            params,
        }
    }
    fn ep(from: &str, to: &str, port: &str) -> Edge {
        Edge {
            from: NodeId::new(from),
            to: NodeId::new(to),
            from_port: Some(port.into()),
        }
    }
    fn status(report: &RunReport, id: &str) -> StepStatus {
        report.step(&NodeId::new(id)).unwrap().status
    }

    #[test]
    fn if_takes_true_branch_and_skips_false() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "i",
                    NodeKind::If,
                    serde_json::json!({ "condition": "t.ok == true" }),
                ),
                n("yes"),
                n("no"),
            ],
            vec![e("t", "i"), ep("i", "yes", "true"), ep("i", "no", "false")],
        );
        let runner = |node: &Node, _: &RunContext| {
            assert_ne!(node.id.0, "i", "le runner ne doit pas voir le node `if`");
            Ok(serde_json::json!({ "ok": true }))
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(status(&report, "i"), StepStatus::Succeeded);
        assert_eq!(
            report.step(&NodeId::new("i")).unwrap().output,
            Some(Value::Bool(true))
        );
        assert_eq!(status(&report, "yes"), StepStatus::Succeeded);
        assert_eq!(status(&report, "no"), StepStatus::Skipped);
    }

    #[test]
    fn switch_selects_matching_branch_then_default() {
        let nodes = || {
            vec![
                trig("t"),
                ctrl(
                    "s",
                    NodeKind::Switch,
                    serde_json::json!({
                        "selector": "t.kind",
                        "cases": [
                            { "value": "a", "port": "pa" },
                            { "value": "b", "port": "pb" }
                        ],
                        "default": "pd"
                    }),
                ),
                n("na"),
                n("nb"),
                n("nd"),
            ]
        };
        let edges = || {
            vec![
                e("t", "s"),
                ep("s", "na", "pa"),
                ep("s", "nb", "pb"),
                ep("s", "nd", "pd"),
            ]
        };

        // Cas correspondant : branche `pb`.
        let d = dag(nodes(), edges());
        let r = run_sequential(&d, &|_: &Node, _: &RunContext| {
            Ok(serde_json::json!({ "kind": "b" }))
        });
        assert_eq!(r.status, RunStatus::Succeeded);
        assert_eq!(status(&r, "nb"), StepStatus::Succeeded);
        assert_eq!(status(&r, "na"), StepStatus::Skipped);
        assert_eq!(status(&r, "nd"), StepStatus::Skipped);

        // Aucun cas → branche par défaut `pd`.
        let d = dag(nodes(), edges());
        let r = run_sequential(&d, &|_: &Node, _: &RunContext| {
            Ok(serde_json::json!({ "kind": "z" }))
        });
        assert_eq!(status(&r, "nd"), StepStatus::Succeeded);
        assert_eq!(status(&r, "na"), StepStatus::Skipped);
        assert_eq!(status(&r, "nb"), StepStatus::Skipped);
    }

    #[test]
    fn if_with_invalid_params_fails_the_run() {
        let d = dag(
            vec![
                trig("t"),
                ctrl("i", NodeKind::If, Value::Null), // pas de `condition`
                n("yes"),
                n("no"),
            ],
            vec![e("t", "i"), ep("i", "yes", "true"), ep("i", "no", "false")],
        );
        let report = run_sequential(&d, &|_: &Node, _: &RunContext| Ok(Value::Null));

        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(status(&report, "i"), StepStatus::Failed);
        assert_eq!(status(&report, "yes"), StepStatus::Skipped);
        assert_eq!(status(&report, "no"), StepStatus::Skipped);
    }

    fn output(report: &RunReport, id: &str) -> Value {
        report
            .step(&NodeId::new(id))
            .unwrap()
            .output
            .clone()
            .unwrap()
    }

    #[test]
    fn loop_iterates_collection_and_aggregates_outputs() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "L",
                    NodeKind::Loop,
                    serde_json::json!({ "items": "t.list" }),
                ),
                n("sink"),
            ],
            vec![e("t", "L"), e("L", "sink")],
        );
        let runner = |node: &Node, ctx: &RunContext| match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "list": [1, 2, 3] })),
            "L" => {
                // Le corps lit l'élément courant exposé par le moteur.
                let item = ctx.output(&NodeId::new("L::item")).expect("item").clone();
                let v = item.as_i64().unwrap();
                Ok(Value::from(v * 2))
            }
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(output(&report, "L"), serde_json::json!([2, 4, 6]));
        assert_eq!(status(&report, "sink"), StepStatus::Succeeded);
        // Les clés synthétiques de boucle ne fuitent pas.
        assert!(report.step(&NodeId::new("L::item")).is_none());
    }

    #[test]
    fn loop_over_empty_collection_runs_successors_once() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "L",
                    NodeKind::Loop,
                    serde_json::json!({ "items": "t.list" }),
                ),
                n("sink"),
            ],
            vec![e("t", "L"), e("L", "sink")],
        );
        let runner = |node: &Node, _: &RunContext| match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "list": [] })),
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(output(&report, "L"), serde_json::json!([]));
        assert_eq!(status(&report, "sink"), StepStatus::Succeeded);
    }

    #[test]
    fn loop_rejects_non_array_items() {
        let d = dag(
            vec![
                trig("t"),
                ctrl("L", NodeKind::Loop, serde_json::json!({ "items": "t.x" })),
            ],
            vec![e("t", "L")],
        );
        let runner = |node: &Node, _: &RunContext| match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "x": 42 })),
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);
        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(status(&report, "L"), StepStatus::Failed);
    }

    #[test]
    fn while_runs_until_condition_false_and_aggregates() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "W",
                    NodeKind::While,
                    serde_json::json!({ "condition": "W::index < 3" }),
                ),
            ],
            vec![e("t", "W")],
        );
        let runner = |node: &Node, ctx: &RunContext| match node.id.0.as_str() {
            "W" => Ok(ctx.output(&NodeId::new("W::index")).unwrap().clone()),
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(output(&report, "W"), serde_json::json!([0, 1, 2]));
    }

    #[test]
    fn while_guard_trips_on_infinite_loop() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "W",
                    NodeKind::While,
                    serde_json::json!({ "condition": "true", "max_iterations": 5 }),
                ),
            ],
            vec![e("t", "W")],
        );
        let report = run_sequential(&d, &|_: &Node, _: &RunContext| Ok(Value::Null));

        assert_eq!(report.status, RunStatus::Failed);
        let w = report.step(&NodeId::new("W")).unwrap();
        assert_eq!(w.status, StepStatus::Failed);
        assert!(w.error.as_deref().unwrap().contains("max_iterations"));
    }
}

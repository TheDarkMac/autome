//! Tests d'intégration **black-box** du moteur (F9.1).
//!
//! Contrairement aux tests unitaires *inline* (qui éprouvent les fonctions
//! privées module par module), ces tests pilotent le moteur via sa **seule
//! API publique** — comme le ferait le worker — sur des workflows composés
//! réalistes : pipeline + mapping, routage conditionnel, boucle + agrégation
//! consommée en aval, diamant fan-out/fan-in, retry/fallback, et équivalence
//! séquentiel ↔ parallèle. Ils servent de garde anti-régression au niveau du
//! contrat du moteur et documentent les workflows tels qu'on les construit.

use autome_core::{Edge, Node, NodeId, NodeKind, WorkflowGraph, WorkflowId};
use autome_engine::{
    run_parallel, run_sequential, Dag, RunContext, RunReport, RunStatus, StepStatus,
};
use serde_json::{json, Value};

// ─────────────────────────── helpers de construction ───────────────────────────

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

fn action(id: &str, params: Value) -> Node {
    Node {
        id: NodeId::new(id),
        name: id.into(),
        kind: NodeKind::Action {
            action_type: "x".into(),
        },
        params,
    }
}

fn ctrl(id: &str, kind: NodeKind, params: Value) -> Node {
    Node {
        id: NodeId::new(id),
        name: id.into(),
        kind,
        params,
    }
}

fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from: NodeId::new(from),
        to: NodeId::new(to),
        from_port: None,
    }
}

fn port_edge(from: &str, to: &str, port: &str) -> Edge {
    Edge {
        from: NodeId::new(from),
        to: NodeId::new(to),
        from_port: Some(port.into()),
    }
}

fn build(nodes: Vec<Node>, edges: Vec<Edge>) -> Dag {
    Dag::from_workflow(&WorkflowGraph {
        id: WorkflowId::new("wf"),
        name: "integration".into(),
        nodes,
        edges,
    })
    .expect("DAG valide")
}

fn status(report: &RunReport, id: &str) -> StepStatus {
    report
        .step(&NodeId::new(id))
        .unwrap_or_else(|| panic!("node `{id}` absent du rapport"))
        .status
}

fn output(report: &RunReport, id: &str) -> Value {
    report
        .step(&NodeId::new(id))
        .and_then(|s| s.output.clone())
        .unwrap_or_else(|| panic!("sortie de `{id}` absente"))
}

// ─────────────────────────────── scénarios ───────────────────────────────

/// Pipeline linéaire trigger → action → action, avec mapping (F2.6) des
/// `params` depuis la sortie du trigger et lecture du prédécesseur.
#[test]
fn linear_pipeline_passes_and_maps_data() {
    let dag = build(
        vec![
            trig("t"),
            action(
                "fetch",
                json!({ "url": "https://api/{{ t.host }}/users/{{ t.id }}" }),
            ),
            action("notify", json!({ "msg": "got {{ fetch.count }} users" })),
        ],
        vec![edge("t", "fetch"), edge("fetch", "notify")],
    );

    let runner = |node: &Node, ctx: &RunContext| match node.id.0.as_str() {
        "t" => Ok(json!({ "host": "example.com", "id": 7 })),
        "fetch" => {
            // Les params arrivent résolus (templating + types).
            assert_eq!(node.params["url"], json!("https://api/example.com/users/7"));
            Ok(json!({ "count": 2 }))
        }
        "notify" => {
            assert_eq!(node.params["msg"], json!("got 2 users"));
            // La sortie du prédécesseur est lisible dans le contexte.
            assert_eq!(
                ctx.output(&NodeId::new("fetch")).unwrap()["count"],
                json!(2)
            );
            Ok(json!({ "sent": true }))
        }
        other => panic!("node inattendu : {other}"),
    };

    let report = run_sequential(&dag, &runner);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(output(&report, "notify"), json!({ "sent": true }));
}

/// Routage conditionnel : seul le port retenu s'exécute, l'autre est `Skipped`.
#[test]
fn conditional_routing_runs_only_taken_branch() {
    let make = |amount: i64| {
        let dag = build(
            vec![
                trig("t"),
                ctrl(
                    "gate",
                    NodeKind::If,
                    json!({ "condition": "t.amount > 100" }),
                ),
                action("approve", Value::Null),
                action("reject", Value::Null),
            ],
            vec![
                edge("t", "gate"),
                port_edge("gate", "approve", "true"),
                port_edge("gate", "reject", "false"),
            ],
        );
        let runner = move |node: &Node, _: &RunContext| match node.id.0.as_str() {
            "t" => Ok(json!({ "amount": amount })),
            _ => Ok(Value::Null),
        };
        run_sequential(&dag, &runner)
    };

    let big = make(250);
    assert_eq!(big.status, RunStatus::Succeeded);
    assert_eq!(status(&big, "approve"), StepStatus::Succeeded);
    assert_eq!(status(&big, "reject"), StepStatus::Skipped);

    let small = make(10);
    assert_eq!(status(&small, "reject"), StepStatus::Succeeded);
    assert_eq!(status(&small, "approve"), StepStatus::Skipped);
}

/// Boucle sur une collection : l'agrégat des itérations est consommé par un
/// node en aval (F2.4.c).
#[test]
fn loop_aggregate_is_consumed_downstream() {
    let dag = build(
        vec![
            trig("t"),
            ctrl("each", NodeKind::Loop, json!({ "items": "t.nums" })),
            action("sum", Value::Null),
        ],
        vec![edge("t", "each"), edge("each", "sum")],
    );

    let runner = |node: &Node, ctx: &RunContext| match node.id.0.as_str() {
        "t" => Ok(json!({ "nums": [1, 2, 3, 4] })),
        "each" => {
            let item = ctx
                .output(&NodeId::new("each::item"))
                .unwrap()
                .as_i64()
                .unwrap();
            Ok(json!(item * item))
        }
        "sum" => {
            let arr = ctx
                .output(&NodeId::new("each"))
                .unwrap()
                .as_array()
                .unwrap();
            let total: i64 = arr.iter().map(|v| v.as_i64().unwrap()).sum();
            Ok(json!({ "total": total }))
        }
        other => panic!("node inattendu : {other}"),
    };

    let report = run_sequential(&dag, &runner);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(output(&report, "each"), json!([1, 4, 9, 16]));
    assert_eq!(output(&report, "sum"), json!({ "total": 30 }));
}

/// Diamant fan-out/fan-in en parallèle : la jointure voit les deux branches.
#[test]
fn parallel_diamond_merges_branch_outputs() {
    let dag = build(
        vec![
            trig("t"),
            action("left", Value::Null),
            action("right", Value::Null),
            action("join", Value::Null),
        ],
        vec![
            edge("t", "left"),
            edge("t", "right"),
            edge("left", "join"),
            edge("right", "join"),
        ],
    );

    let runner = |node: &Node, ctx: &RunContext| match node.id.0.as_str() {
        "join" => {
            let l = ctx.output(&NodeId::new("left")).expect("left");
            let r = ctx.output(&NodeId::new("right")).expect("right");
            Ok(json!({ "l": l, "r": r }))
        }
        other => Ok(json!(other)),
    };

    let report = run_parallel(&dag, &runner, 4);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(
        output(&report, "join"),
        json!({ "l": "left", "r": "right" })
    );
}

/// Retry transitoire puis succès, de bout en bout (F2.7.a).
#[test]
fn retry_recovers_then_pipeline_completes() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dag = build(
        vec![
            trig("t"),
            action("flaky", json!({ "_error": { "max_attempts": 4 } })),
            action("done", Value::Null),
        ],
        vec![edge("t", "flaky"), edge("flaky", "done")],
    );

    let attempts = AtomicUsize::new(0);
    let runner = |node: &Node, _: &RunContext| match node.id.0.as_str() {
        "flaky" => {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(format!("essai {n} KO"))
            } else {
                Ok(json!("ok"))
            }
        }
        _ => Ok(Value::Null),
    };

    let report = run_sequential(&dag, &runner);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(status(&report, "flaky"), StepStatus::Succeeded);
    assert_eq!(status(&report, "done"), StepStatus::Succeeded);
    assert_eq!(attempts.load(Ordering::SeqCst), 3); // 2 KO + 1 OK
}

/// Échec routé vers un handler `error` : le workflow réussit globalement,
/// la sortie par défaut est court-circuitée (F2.7.c).
#[test]
fn error_fallback_routes_to_handler() {
    let dag = build(
        vec![
            trig("t"),
            action("call", json!({ "_error": { "on_error": "fallback" } })),
            action("normal", Value::Null),
            action("recover", Value::Null),
        ],
        vec![
            edge("t", "call"),
            edge("call", "normal"),
            port_edge("call", "recover", "error"),
        ],
    );

    let runner = |node: &Node, _: &RunContext| {
        if node.id.0 == "call" {
            Err("503 upstream".into())
        } else {
            Ok(Value::Null)
        }
    };

    for report in [
        run_sequential(&dag, &runner),
        run_parallel(&dag, &runner, 3),
    ] {
        assert_eq!(report.status, RunStatus::Succeeded, "échec géré ⇒ succès");
        assert_eq!(status(&report, "call"), StepStatus::Failed);
        assert_eq!(status(&report, "recover"), StepStatus::Succeeded);
        assert_eq!(status(&report, "normal"), StepStatus::Skipped);
    }
}

/// Échec **non géré** au milieu du graphe : fail-fast, aval `Skipped`, run
/// `Failed` — identique en séquentiel et en parallèle.
#[test]
fn unhandled_failure_fails_fast_in_both_engines() {
    let dag = build(
        vec![
            trig("t"),
            action("a", Value::Null),
            action("b", Value::Null),
        ],
        vec![edge("t", "a"), edge("a", "b")],
    );
    let runner = |node: &Node, _: &RunContext| {
        if node.id.0 == "a" {
            Err("boom".into())
        } else {
            Ok(Value::Null)
        }
    };

    for report in [
        run_sequential(&dag, &runner),
        run_parallel(&dag, &runner, 2),
    ] {
        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(status(&report, "a"), StepStatus::Failed);
        assert_eq!(status(&report, "b"), StepStatus::Skipped);
    }
}

/// Propriété : sur un graphe large (fan-out → fan-in), `run_parallel` produit
/// **exactement** le même statut par node et le même statut global que
/// `run_sequential` (sémantique identique, F2.5).
#[test]
fn parallel_matches_sequential_on_wide_graph() {
    const N: usize = 30;
    let mut nodes = vec![trig("t"), action("join", Value::Null)];
    let mut edges = Vec::new();
    for i in 0..N {
        let id = format!("b{i}");
        nodes.push(action(&id, Value::Null));
        edges.push(edge("t", &id));
        edges.push(edge(&id, "join"));
    }
    let dag = build(nodes, edges);

    // Sortie déterministe (indépendante de l'ordonnancement).
    let runner = |node: &Node, _: &RunContext| Ok(json!(node.id.0));

    let seq = run_sequential(&dag, &runner);
    let par = run_parallel(&dag, &runner, 8);

    assert_eq!(seq.status, par.status);
    assert_eq!(seq.status, RunStatus::Succeeded);
    for i in 0..N {
        let id = format!("b{i}");
        assert_eq!(status(&seq, &id), status(&par, &id), "divergence sur {id}");
    }
    assert_eq!(status(&par, "join"), StepStatus::Succeeded);
}

/// Validation black-box : un workflow cyclique est **rejeté à la
/// construction** (F2.1.c) — le moteur ne tourne jamais sur un cycle.
#[test]
fn cyclic_workflow_is_rejected_at_construction() {
    let result = Dag::from_workflow(&WorkflowGraph {
        id: WorkflowId::new("wf"),
        name: "cycle".into(),
        nodes: vec![
            trig("t"),
            action("a", Value::Null),
            action("b", Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "a")],
    });
    assert!(result.is_err(), "un cycle doit être rejeté");
}

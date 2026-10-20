//! Trace d'un run (#249), dans son propre binaire de test (#259).
//!
//! Ce test installe un subscriber pour son seul thread (`set_default`) et
//! vérifie les spans des nœuds, créés sur le thread bloquant du moteur. Or le
//! cache d'intérêt des callsites de `tracing` est **global au processus** :
//! parmi les tests de `executor`, un autre test qui passait le premier par le
//! callsite du span de nœud pendant l'enregistrement de ce subscriber pouvait
//! le figer à « jamais », et les spans de nœud manquaient (test instable sous
//! `cargo test` parallèle). Seul dans son binaire, aucun autre test ne partage
//! ce cache.

use std::sync::Arc;

use autome_core::telemetry::{subscriber, LogFormat};
use autome_core::{
    Delivery, DeliveryId, Edge, InMemoryRunStateStore, InMemoryWorkflowRepository, Message, Node,
    NodeId, NodeKind, RunCheckpoint, RunStateStore, WorkflowGraph, WorkflowId, WorkflowRepository,
    LEGACY_NAMESPACE,
};
use autome_worker::{EnvCredentialResolver, Handler, HandlerVerdict, RunExecutor};
use opentelemetry::trace::{SpanId, Status, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;
const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT_ID: &str = "00f067aa0ba902b7";

/// `entry` (trigger manuel) → `act`, action inconnue du worker : le run
/// échoue sur `act`.
fn failing_workflow(id: &str) -> WorkflowGraph {
    let node = |id: &str, kind: NodeKind| Node {
        position: None,
        id: NodeId::new(id),
        name: id.into(),
        kind,
        params: Value::Null,
    };
    WorkflowGraph {
        id: WorkflowId::new(id),
        name: id.into(),
        nodes: vec![
            node(
                "entry",
                NodeKind::Trigger {
                    trigger_type: "manual".into(),
                },
            ),
            node(
                "act",
                NodeKind::Action {
                    action_type: "slack_send".into(),
                },
            ),
        ],
        edges: vec![Edge {
            from: NodeId::new("entry"),
            to: NodeId::new("act"),
            from_port: None,
        }],
    }
}

#[tokio::test]
async fn the_run_continues_the_trace_of_its_dispatch() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let _default = tracing::subscriber::set_default(subscriber(
        LogFormat::Json,
        tracing_subscriber::EnvFilter::new("info"),
        std::io::sink,
        Some(provider.tracer("test")),
    ));
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let store = Arc::new(InMemoryRunStateStore::new());
    repo.create(LEGACY_NAMESPACE, failing_workflow("wf-traced"))
        .await
        .unwrap();
    store
        .checkpoint(&RunCheckpoint::queued(
            "run-traced",
            "wf-traced",
            1,
            "manual",
            NOW,
        ))
        .await
        .unwrap();
    let exec = RunExecutor::new(repo, store, Arc::new(EnvCredentialResolver));
    let mut message = Message::new(json!({
        "run_id": "run-traced",
        "workflow_id": "wf-traced",
        "version": 1,
        "trigger_type": "manual",
    }));
    message.headers.insert(
        "traceparent".into(),
        format!("00-{TRACE_ID}-{PARENT_ID}-01"),
    );
    let delivery = Delivery {
        id: DeliveryId::new(1),
        queue: "runs".into(),
        message,
        redeliveries: 0,
    };

    assert_eq!(exec.handle(&delivery).await, HandlerVerdict::DeadLetter);

    let spans = exporter.get_finished_spans().unwrap();
    let find = |name: &str| {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("span `{name}` absent : {spans:?}"))
    };
    let run = find("run");
    assert_eq!(run.span_context.trace_id().to_string(), TRACE_ID);
    assert_eq!(run.parent_span_id.to_string(), PARENT_ID);
    assert!(
        matches!(run.status, Status::Error { .. }),
        "{:?}",
        run.status
    );
    // Les nœuds, exécutés sur le thread bloquant du moteur, sont ses enfants.
    for node in ["node trigger", "node slack_send"] {
        let node = find(node);
        assert_eq!(node.parent_span_id, run.span_context.span_id());
        assert_ne!(node.parent_span_id, SpanId::INVALID);
    }
    assert!(matches!(
        find("node slack_send").status,
        Status::Error { .. }
    ));
}

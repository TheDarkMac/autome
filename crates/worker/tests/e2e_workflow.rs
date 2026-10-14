//! Tests **end-to-end** du pipeline d'exécution complet (F9.4).
//!
//! Assemble les vraies briques de production via leurs API publiques et
//! fait transiter un workflow réaliste de bout en bout :
//!
//! ```text
//! QueueRunDispatcher  →  Queue  →  WorkerPool  →  Engine  →  RunStateStore
//!   (mécanisme API)      (F5.1)     (F5.2)        (F2.2)       (F5.4)
//! ```
//!
//! Là où F9.3 prouve la *surface HTTP* du déclenchement (le run reste
//! `Queued`), F9.4 prouve que le run **s'exécute réellement jusqu'au bout**.
//! On entre par le `QueueRunDispatcher` — exactement ce que le handler HTTP
//! `POST /workflows/:id/runs` invoque en interne.
//!
//! Le *handler* worker assemblé ici (charger le workflow → moteur →
//! persister l'état) matérialise le travail que `autome-worker` câblera sur
//! le transport réel ; tant qu'il n'existe pas en production, le construire
//! dans le test e2e est la façon de prouver que les pièces s'emboîtent.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use autome_core::{
    dlq_of, DispatchRequest, InMemoryQueue, InMemoryRunStateStore, InMemoryWorkflowRepository,
    Node, NodeId, NodeKind, Queue, QueueRunDispatcher, RunDispatcher, RunStateStore, RunStatus,
    WorkflowGraph, WorkflowId, WorkflowRepository,
};
use autome_engine::{Dag, Engine, NodeRunner, RunStatus as EngineRunStatus, StepStatus};
use autome_worker::{
    Handler, HandlerVerdict, InMemoryIdempotencyStore, ProcessOutcome, WorkerPool,
};
use serde_json::{json, Value};

const QUEUE: &str = "runs";
const NOW_MS: i64 = 1_700_000_000_000;

/// Workflow réaliste à trois nodes avec **mapping de données par templating
/// (#160)** : `entry` (trigger) → `fetch` (produit `{ value: 21 }`) → `double`
/// (lit la sortie de `fetch` **via `{{ fetch.value }}` dans ses `params`** et
/// produit `{ result: 42 }`). Le moteur résout le template **avant** d'appeler
/// le runner ; ce dernier ne fait que lire `node.params` déjà résolu — preuve
/// que le chaînage inter-nodes fonctionne de bout en bout côté worker.
fn pipeline_workflow(id: &str) -> WorkflowGraph {
    use autome_core::Edge;
    WorkflowGraph {
        id: WorkflowId::new(id),
        name: format!("pipeline {id}"),
        nodes: vec![
            Node {
                id: NodeId::new("entry"),
                name: "entry".into(),
                kind: NodeKind::Trigger {
                    trigger_type: "manual".into(),
                },
                params: Value::Null,
            },
            Node {
                id: NodeId::new("fetch"),
                name: "fetch".into(),
                kind: NodeKind::Action {
                    action_type: "produce".into(),
                },
                params: Value::Null,
            },
            Node {
                id: NodeId::new("double"),
                name: "double".into(),
                kind: NodeKind::Action {
                    action_type: "transform".into(),
                },
                // Référence templatée à la sortie du node amont `fetch`. Le
                // type est préservé (expression unique → entier, pas chaîne).
                params: json!({ "input": "{{ fetch.value }}" }),
            },
        ],
        edges: vec![
            Edge {
                from: NodeId::new("entry"),
                to: NodeId::new("fetch"),
                from_port: None,
            },
            Edge {
                from: NodeId::new("fetch"),
                to: NodeId::new("double"),
                from_port: None,
            },
        ],
    }
}

/// Runner de nodes : `fetch` produit `21`, `double` lit son entrée **depuis
/// `node.params["input"]` déjà résolu par le moteur** (templating #160) et la
/// double. Tout node `name == "boom"` échoue (chemin d'erreur). `runs` compte
/// les exécutions effectives (pour l'idempotence).
fn make_runner(runs: Arc<AtomicU32>) -> impl NodeRunner + Sync {
    move |node: &Node, _ctx: &autome_engine::RunContext| -> Result<Value, String> {
        match node.name.as_str() {
            "boom" => Err("échec simulé du node".into()),
            "fetch" => {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "value": 21 }))
            }
            "double" => {
                // `{{ fetch.value }}` a été résolu en l'entier `21` (type
                // préservé) avant d'atteindre le runner.
                let input = node
                    .params
                    .get("input")
                    .and_then(Value::as_i64)
                    .ok_or("double : `params.input` (templaté depuis `fetch`) absent")?;
                Ok(json!({ "result": input * 2 }))
            }
            _ => Ok(Value::Null),
        }
    }
}

/// Handler worker e2e : décode le message → charge le workflow → exécute le
/// moteur → persiste l'état (`Running` puis `Succeeded`/`Failed`) avec les
/// sorties des nodes. Verdict `Ok` si succès, `DeadLetter` si échec.
struct RunExecutor {
    repo: Arc<InMemoryWorkflowRepository>,
    store: Arc<InMemoryRunStateStore>,
    runs: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl Handler for RunExecutor {
    async fn handle(&self, delivery: &autome_core::Delivery) -> HandlerVerdict {
        let body = &delivery.message.body;
        let run_id = body["run_id"].as_str().expect("run_id dans le message");
        let workflow_id = body["workflow_id"].as_str().expect("workflow_id");

        let workflow = self
            .repo
            .get(&WorkflowId::new(workflow_id))
            .await
            .expect("repo get")
            .expect("workflow connu");
        let dag = Dag::from_workflow(&workflow).expect("DAG valide");

        // Transition Queued → Running.
        let mut checkpoint = self
            .store
            .load(run_id)
            .await
            .expect("load")
            .expect("run connu");
        checkpoint.status = RunStatus::Running;
        checkpoint.started_at_ms = Some(NOW_MS);
        self.store
            .checkpoint(&checkpoint)
            .await
            .expect("checkpoint running");

        // Exécution du moteur.
        let runner = make_runner(Arc::clone(&self.runs));
        let report = Engine::new().run(&dag, &runner);

        // Persiste les sorties des nodes réussis + le statut final.
        for step in &report.steps {
            if step.status == StepStatus::Succeeded {
                if let Some(out) = &step.output {
                    checkpoint
                        .outputs
                        .insert(step.node_id.0.clone(), out.clone());
                }
            }
        }
        checkpoint.status = report.status;
        checkpoint.finished_at_ms = Some(NOW_MS);
        if report.status == EngineRunStatus::Failed {
            checkpoint.error = report
                .steps
                .iter()
                .find_map(|s| s.error.clone())
                .or_else(|| Some("run en échec".into()));
        }
        self.store
            .checkpoint(&checkpoint)
            .await
            .expect("checkpoint final");

        match report.status {
            EngineRunStatus::Succeeded => HandlerVerdict::Ok,
            _ => HandlerVerdict::DeadLetter,
        }
    }
}

/// Stack e2e assemblée : repo + store + queue + dispatcher + pool.
struct Stack {
    repo: Arc<InMemoryWorkflowRepository>,
    store: Arc<InMemoryRunStateStore>,
    queue: Arc<InMemoryQueue>,
    dispatcher: QueueRunDispatcher,
    runs: Arc<AtomicU32>,
}

impl Stack {
    fn new() -> Self {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let store = Arc::new(InMemoryRunStateStore::new());
        let queue = Arc::new(InMemoryQueue::new());
        let dispatcher = QueueRunDispatcher::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            Arc::clone(&store) as Arc<dyn RunStateStore + Send + Sync>,
        );
        Self {
            repo,
            store,
            queue,
            dispatcher,
            runs: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Construit le pool worker câblé sur le handler e2e.
    fn pool(&self, concurrency: usize) -> WorkerPool {
        let handler = Arc::new(RunExecutor {
            repo: Arc::clone(&self.repo),
            store: Arc::clone(&self.store),
            runs: Arc::clone(&self.runs),
        });
        WorkerPool::new(
            Arc::clone(&self.queue) as Arc<dyn Queue + Send + Sync>,
            handler,
            Arc::new(InMemoryIdempotencyStore::new()),
            QUEUE,
            concurrency,
        )
        .with_idle_sleep(Duration::from_millis(2))
    }

    /// Dispatche un run pour `workflow_id` (le workflow doit déjà exister).
    async fn dispatch(&self, workflow_id: &str) -> String {
        self.dispatcher
            .dispatch(DispatchRequest {
                workflow_id: workflow_id.into(),
                version: 1,
                trigger_type: "manual".into(),
                payload: json!({ "seed": true }),
                now_ms: NOW_MS,
                owner_id: None,
            })
            .await
            .expect("dispatch")
            .run_id
    }
}

// ───────────────────────── F9.4.b — chemin nominal ─────────────────────

#[tokio::test]
async fn dispatched_workflow_runs_to_completion_with_persisted_outputs() {
    let stack = Stack::new();
    stack
        .repo
        .create(pipeline_workflow("wf-pipe"), None)
        .await
        .unwrap();

    // À l'instant du dispatch, le run est Queued et la queue contient 1 msg.
    let run_id = stack.dispatch("wf-pipe").await;
    assert_eq!(
        stack.store.load(&run_id).await.unwrap().unwrap().status,
        RunStatus::Queued
    );
    assert_eq!(stack.queue.depth(QUEUE).await.unwrap(), 1);

    // Un seul `process_one` draine et exécute le run de bout en bout.
    let pool = stack.pool(1);
    let outcome = pool.process_one().await.unwrap();
    assert_eq!(outcome, ProcessOutcome::Handled(HandlerVerdict::Ok));

    // État final : Succeeded, sorties des deux actions persistées (la donnée
    // `21` a transité de `fetch` vers `double` via le **templating** `{{
    // fetch.value }}` résolu par le moteur côté worker → `42`, #160).
    let final_state = stack.store.load(&run_id).await.unwrap().unwrap();
    assert_eq!(final_state.status, RunStatus::Succeeded);
    assert_eq!(final_state.outputs["fetch"], json!({ "value": 21 }));
    assert_eq!(final_state.outputs["double"], json!({ "result": 42 }));
    assert_eq!(final_state.started_at_ms, Some(NOW_MS));
    assert_eq!(final_state.finished_at_ms, Some(NOW_MS));

    // Queue drainée (message acké), runner appelé une fois pour `fetch`.
    assert_eq!(stack.queue.depth(QUEUE).await.unwrap(), 0);
    assert_eq!(stack.runs.load(Ordering::SeqCst), 1);
}

// ───────────────────────── F9.4.c — chemin d'échec ─────────────────────

#[tokio::test]
async fn failing_node_marks_run_failed_and_dead_letters_message() {
    let stack = Stack::new();
    // Workflow dont le second node échoue (`name == "boom"`).
    let mut wf = pipeline_workflow("wf-boom");
    wf.nodes[1].name = "boom".into();
    stack.repo.create(wf, None).await.unwrap();

    let run_id = stack.dispatch("wf-boom").await;
    let pool = stack.pool(1);
    let outcome = pool.process_one().await.unwrap();
    assert_eq!(outcome, ProcessOutcome::Handled(HandlerVerdict::DeadLetter));

    // Run marqué Failed avec un message d'erreur.
    let final_state = stack.store.load(&run_id).await.unwrap().unwrap();
    assert_eq!(final_state.status, RunStatus::Failed);
    assert!(
        final_state.error.is_some(),
        "un message d'erreur est attendu"
    );

    // Le message a quitté la queue principale pour la DLQ.
    assert_eq!(stack.queue.depth(QUEUE).await.unwrap(), 0);
    assert_eq!(stack.queue.depth(&dlq_of(QUEUE)).await.unwrap(), 1);
}

// ───────────────────────── F9.4.d — pool concurrent + idempotence ──────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pool_drains_all_dispatched_runs_to_success() {
    let stack = Stack::new();
    stack
        .repo
        .create(pipeline_workflow("wf-pipe"), None)
        .await
        .unwrap();

    const N: usize = 50;
    let mut run_ids: Vec<String> = Vec::with_capacity(N);
    for _ in 0..N {
        run_ids.push(stack.dispatch("wf-pipe").await);
    }
    assert_eq!(stack.queue.depth(QUEUE).await.unwrap(), N);

    // 4 tâches consomment la queue en parallèle.
    let running = stack.pool(4).start();

    // Attend que tous les runs soient Succeeded (timeout de garde).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut done = 0usize;
        for id in &run_ids {
            if stack
                .store
                .load(id)
                .await
                .unwrap()
                .map(|c| c.status == RunStatus::Succeeded)
                .unwrap_or(false)
            {
                done += 1;
            }
        }
        if done == N {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "seulement {done}/{N} runs terminés avant le timeout"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    running.stop().await;

    // Queue entièrement drainée et chaque run exécuté exactement une fois.
    assert_eq!(stack.queue.depth(QUEUE).await.unwrap(), 0);
    assert_eq!(stack.runs.load(Ordering::SeqCst), N as u32);
}

#[tokio::test]
async fn duplicate_delivery_is_deduplicated_and_executes_once() {
    let stack = Stack::new();
    stack
        .repo
        .create(pipeline_workflow("wf-pipe"), None)
        .await
        .unwrap();

    // Deux dispatchs avec le *même* run_id → même `idempotency-key`, deux
    // messages sur la queue (le store est ré-écrit en Queued à l'identique).
    let fixed = QueueRunDispatcher::new(
        Arc::clone(&stack.queue) as Arc<dyn Queue + Send + Sync>,
        Arc::clone(&stack.store) as Arc<dyn RunStateStore + Send + Sync>,
    )
    .with_id_generator(|| "run-dup".to_string());
    let req = || DispatchRequest {
        workflow_id: "wf-pipe".into(),
        version: 1,
        trigger_type: "manual".into(),
        payload: Value::Null,
        now_ms: NOW_MS,
        owner_id: None,
    };
    fixed.dispatch(req()).await.unwrap();
    fixed.dispatch(req()).await.unwrap();
    assert_eq!(stack.queue.depth(QUEUE).await.unwrap(), 2);

    let pool = stack.pool(1);
    // 1ʳᵉ livraison : exécutée.
    assert_eq!(
        pool.process_one().await.unwrap(),
        ProcessOutcome::Handled(HandlerVerdict::Ok)
    );
    // 2ᵉ livraison (même clé) : dédupliquée, handler non rappelé.
    assert_eq!(
        pool.process_one().await.unwrap(),
        ProcessOutcome::Deduplicated
    );

    assert_eq!(
        stack.store.load("run-dup").await.unwrap().unwrap().status,
        RunStatus::Succeeded
    );
    assert_eq!(
        stack.runs.load(Ordering::SeqCst),
        1,
        "exécuté une seule fois"
    );
}

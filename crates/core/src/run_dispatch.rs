//! Déclenchement d'un run (F7.2.a).
//!
//! Abstrait l'opération « démarrer un workflow » derrière un trait
//! [`RunDispatcher`] consommable par l'API publique (F7.2.b). L'impl
//! [`QueueRunDispatcher`] est la version « production » : elle enregistre le
//! `RunCheckpoint { status: Queued }` **et** son message de file dans le
//! [`RunStateStore`], en une transaction (pattern Outbox, #239), puis publie
//! le message sur la [`Queue`] que le worker pool consommera (F5.2). Une
//! publication perdue (broker coupé, process tué) n'abandonne plus le run :
//! le relais de l'outbox du worker republie les messages restés en attente.
//! Les tests des handlers HTTP utilisent un mock pour observer les appels
//! sans dépendre du chemin queue + worker.
//!
//! Le contrat est volontairement étroit (`dispatch` renvoie le `run_id`
//! généré, l'API reste agnostique du format d'identifiant — string en
//! attendant les UUID PG).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{warn, Instrument};

use crate::queue::{Message, Queue};
use crate::run_state::{outbox_idempotency_key, OutboxMessage, RunCheckpoint, RunStateStore};
use crate::Result;

/// Description d'un nouveau run à dispatcher.
#[derive(Debug, Clone)]
pub struct DispatchRequest {
    /// Identifiant du workflow à exécuter.
    pub workflow_id: String,
    /// Version exacte du workflow (sera figée pour la durée du run).
    pub version: i32,
    /// Type de trigger (`"manual"` pour un déclenchement API).
    pub trigger_type: String,
    /// Payload libre transmis au workflow (entrées initiales).
    pub payload: Value,
    /// Timestamp Unix (ms) — figé par le caller pour permettre les tests
    /// déterministes (le handler API lit son `Clock` injecté).
    pub now_ms: i64,
    /// Propriétaire du run (celui du workflow), porté par le checkpoint
    /// initial — cf. [`RunCheckpoint::owner_id`].
    pub owner_id: Option<String>,
}

/// Réponse d'un dispatch — `run_id` créé + état initial persisté.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    /// Identifiant unique du run, à exposer à l'appelant HTTP.
    pub run_id: String,
}

/// Contrat de déclenchement d'un run. L'API consomme un
/// `Arc<dyn RunDispatcher + Send + Sync>` ; les tests utilisent un mock.
#[async_trait]
pub trait RunDispatcher: Send + Sync {
    /// Démarre un nouveau run et renvoie son identifiant. Persiste
    /// l'état initial (Queued) et son message, puis le publie.
    async fn dispatch(&self, req: DispatchRequest) -> Result<DispatchOutcome>;
}

// ────────────────────── QueueRunDispatcher ──────────────────────

/// Impl par défaut : persistance + publication sur queue.
///
/// Le générateur d'`id` est injectable pour rendre les tests déterministes.
/// L'impl par défaut tire `run-<32 hex>` (128 bits aléatoires, cf.
/// [`random_run_id`]).
pub struct QueueRunDispatcher {
    queue: Arc<dyn Queue + Send + Sync>,
    queue_name: String,
    store: Arc<dyn RunStateStore + Send + Sync>,
    id_gen: Arc<dyn Fn() -> String + Send + Sync>,
}

impl QueueRunDispatcher {
    /// Construit un dispatcher câblé sur une `queue` + un `store`.
    /// `queue_name` est la queue cible (par défaut `"runs"`).
    pub fn new(
        queue: Arc<dyn Queue + Send + Sync>,
        store: Arc<dyn RunStateStore + Send + Sync>,
    ) -> Self {
        Self {
            queue,
            queue_name: "runs".to_string(),
            store,
            id_gen: Arc::new(random_run_id),
        }
    }

    /// Builder : remplace le nom de queue cible.
    pub fn with_queue_name(mut self, name: impl Into<String>) -> Self {
        self.queue_name = name.into();
        self
    }

    /// Builder : remplace le générateur d'id (test-only en général).
    pub fn with_id_generator<F>(mut self, gen: F) -> Self
    where
        F: Fn() -> String + Send + Sync + 'static,
    {
        self.id_gen = Arc::new(gen);
        self
    }
}

#[async_trait]
impl RunDispatcher for QueueRunDispatcher {
    async fn dispatch(&self, req: DispatchRequest) -> Result<DispatchOutcome> {
        let run_id = (self.id_gen)();
        // Span producteur (#249) : son contexte voyage dans les en-têtes du
        // message, le span `run` du worker en sera l'enfant.
        let span = tracing::info_span!(
            crate::telemetry::DISPATCH_SPAN,
            otel.name = "run dispatch",
            otel.kind = "producer",
            run_id = %run_id,
            workflow_id = %req.workflow_id,
            trigger_type = %req.trigger_type,
        );
        self.dispatch_as(run_id, req).instrument(span).await
    }
}

impl QueueRunDispatcher {
    async fn dispatch_as(&self, run_id: String, req: DispatchRequest) -> Result<DispatchOutcome> {
        let checkpoint = RunCheckpoint::queued(
            &run_id,
            &req.workflow_id,
            req.version,
            &req.trigger_type,
            req.now_ms,
        )
        .with_owner(req.owner_id);
        let body = json!({
            "run_id": &run_id,
            "workflow_id": &req.workflow_id,
            "version": req.version,
            "trigger_type": &req.trigger_type,
            "payload": req.payload,
        });
        let mut message = Message::new(body)
            .with_header("idempotency-key", outbox_idempotency_key(&run_id, 0))
            .with_header("trigger-source", &req.trigger_type);
        // Conservé par l'outbox : un message republié (relais, remise en
        // file) garde la trace d'origine.
        crate::telemetry::inject_context(&mut message.headers);
        let outbox = OutboxMessage {
            run_id: run_id.clone(),
            queue: self.queue_name.clone(),
            generation: 0,
            message,
        };

        // 1. Run et message ensemble : une fois écrits, le run partira.
        self.store.enqueue(&checkpoint, &outbox).await?;

        // 2. Publication immédiate. Un échec n'est pas celui du dispatch : le
        //    relais de l'outbox republiera le message resté en attente.
        match self.queue.publish(&self.queue_name, outbox.message).await {
            Ok(()) => {
                if let Err(e) = self.store.mark_published(&run_id, 0, req.now_ms).await {
                    // Message republié par le relais : doublon écarté par le
                    // claim du worker.
                    warn!(run_id, error = %e, "publication du run non marquée dans l'outbox");
                }
            }
            Err(e) => warn!(
                run_id,
                error = %e,
                "publication du run différée : le relais de l'outbox la reprendra"
            ),
        }

        Ok(DispatchOutcome { run_id })
    }
}

/// Identifiant de run aléatoire : `run-` + 128 bits en hex (CSPRNG).
///
/// Remplace un compteur process-local qui (1) repartait à 1 à chaque
/// redémarrage et entre répliques — l'upsert de `checkpoint` écrasait alors
/// un run existant — et (2) rendait les ids devinables, donc énumérables.
pub fn random_run_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("run-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::InMemoryQueue;
    use crate::run_state::{InMemoryRunStateStore, RunStatus};

    fn make_dispatcher() -> (
        QueueRunDispatcher,
        Arc<InMemoryQueue>,
        Arc<InMemoryRunStateStore>,
    ) {
        let queue: Arc<InMemoryQueue> = Arc::new(InMemoryQueue::new());
        let store: Arc<InMemoryRunStateStore> = Arc::new(InMemoryRunStateStore::new());
        let dispatcher = QueueRunDispatcher::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            Arc::clone(&store) as Arc<dyn RunStateStore + Send + Sync>,
        )
        .with_id_generator(|| "fixed-run-id".to_string());
        (dispatcher, queue, store)
    }

    fn sample_request() -> DispatchRequest {
        DispatchRequest {
            workflow_id: "wf-1".into(),
            version: 1,
            trigger_type: "manual".into(),
            payload: json!({ "input": "hello" }),
            now_ms: 1_700_000_000_000,
            owner_id: Some("alice".into()),
        }
    }

    #[tokio::test]
    async fn dispatch_returns_run_id_from_id_generator() {
        let (d, _, _) = make_dispatcher();
        let outcome = d.dispatch(sample_request()).await.unwrap();
        assert_eq!(outcome.run_id, "fixed-run-id");
    }

    #[tokio::test]
    async fn dispatch_persists_checkpoint_in_queued_state() {
        let (d, _, store) = make_dispatcher();
        d.dispatch(sample_request()).await.unwrap();
        let snap = store.load("fixed-run-id").await.unwrap().expect("stored");
        assert_eq!(snap.run_id, "fixed-run-id");
        assert_eq!(snap.workflow_id, "wf-1");
        assert_eq!(snap.version, 1);
        assert_eq!(snap.trigger_type, "manual");
        assert_eq!(snap.status, RunStatus::Queued);
        assert_eq!(snap.created_at_ms, 1_700_000_000_000);
        assert!(snap.started_at_ms.is_none());
        assert!(snap.next_node.is_none());
        assert_eq!(snap.owner_id.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn dispatch_publishes_message_with_run_payload_on_runs_queue() {
        let (d, queue, _) = make_dispatcher();
        d.dispatch(sample_request()).await.unwrap();
        assert_eq!(queue.depth("runs").await.unwrap(), 1);
        let delivery = queue.poll("runs").await.unwrap().expect("message");
        assert_eq!(delivery.message.body["run_id"], "fixed-run-id");
        assert_eq!(delivery.message.body["workflow_id"], "wf-1");
        assert_eq!(delivery.message.body["trigger_type"], "manual");
        assert_eq!(
            delivery.message.body["payload"],
            json!({ "input": "hello" })
        );
        // idempotency-key = run_id pour que le worker dédup F5.2 fonctionne
        assert_eq!(
            delivery
                .message
                .headers
                .get("idempotency-key")
                .map(String::as_str),
            Some("fixed-run-id")
        );
        assert_eq!(
            delivery
                .message
                .headers
                .get("trigger-source")
                .map(String::as_str),
            Some("manual")
        );
    }

    #[tokio::test]
    async fn traced_dispatch_carries_the_trace_context_into_the_outbox_message() {
        use crate::telemetry::{subscriber, LogFormat};
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

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
        let (d, queue, _) = make_dispatcher();
        d.dispatch(sample_request()).await.unwrap();
        // Broker coupé : le message reste dans l'outbox, que republient le
        // relais et les remises en file.
        let store: Arc<InMemoryRunStateStore> = Arc::new(InMemoryRunStateStore::new());
        QueueRunDispatcher::new(
            Arc::new(DownQueue),
            Arc::clone(&store) as Arc<dyn RunStateStore + Send + Sync>,
        )
        .dispatch(sample_request())
        .await
        .unwrap();

        let dispatch = exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .find(|s| {
                s.name == "run dispatch"
                    && s.attributes
                        .iter()
                        .any(|kv| kv.value.as_str() == "fixed-run-id")
            })
            .expect("span de mise en file exporté");
        assert_eq!(dispatch.span_kind, opentelemetry::trace::SpanKind::Producer);
        let expected = format!(
            "00-{}-{}-01",
            dispatch.span_context.trace_id(),
            dispatch.span_context.span_id()
        );
        let delivery = queue.poll("runs").await.unwrap().expect("message");
        assert_eq!(delivery.message.headers.get("traceparent"), Some(&expected));
        let pending = store.pending_outbox(i64::MAX, 10).await.unwrap();
        assert_eq!(pending.len(), 1);
        let traceparent = pending[0].message.headers.get("traceparent");
        assert!(
            traceparent.is_some_and(|t| t.starts_with("00-") && t != &expected),
            "contexte de sa propre mise en file : {traceparent:?}"
        );
    }

    #[tokio::test]
    async fn untraced_dispatch_adds_no_trace_header() {
        let (d, queue, _) = make_dispatcher();
        d.dispatch(sample_request()).await.unwrap();
        let delivery = queue.poll("runs").await.unwrap().expect("message");
        assert!(!delivery.message.headers.contains_key("traceparent"));
    }

    #[tokio::test]
    async fn with_queue_name_targets_a_custom_queue() {
        let queue: Arc<InMemoryQueue> = Arc::new(InMemoryQueue::new());
        let store: Arc<InMemoryRunStateStore> = Arc::new(InMemoryRunStateStore::new());
        let d = QueueRunDispatcher::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            Arc::clone(&store) as Arc<dyn RunStateStore + Send + Sync>,
        )
        .with_queue_name("manual-runs")
        .with_id_generator(|| "x".into());
        d.dispatch(sample_request()).await.unwrap();
        assert_eq!(queue.depth("manual-runs").await.unwrap(), 1);
        assert_eq!(queue.depth("runs").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn default_generator_produces_random_unique_ids() {
        let queue: Arc<InMemoryQueue> = Arc::new(InMemoryQueue::new());
        let store: Arc<InMemoryRunStateStore> = Arc::new(InMemoryRunStateStore::new());
        let d = QueueRunDispatcher::new(
            Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
            Arc::clone(&store) as Arc<dyn RunStateStore + Send + Sync>,
        );
        let o1 = d.dispatch(sample_request()).await.unwrap();
        let o2 = d.dispatch(sample_request()).await.unwrap();
        assert_ne!(o1.run_id, o2.run_id);
        // Format `run-<32 hex>` (128 bits aléatoires, non séquentiels).
        assert!(o1.run_id.starts_with("run-"), "id = {}", o1.run_id);
        assert_eq!(o1.run_id.len(), 4 + 32);
        assert!(o1.run_id[4..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn dispatcher_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<QueueRunDispatcher>();
    }

    /// File dont la publication échoue (broker coupé).
    struct DownQueue;

    #[async_trait]
    impl Queue for DownQueue {
        async fn publish(&self, _: &str, _: Message) -> Result<()> {
            Err(crate::Error::Invalid("broker injoignable".into()))
        }
        async fn poll(&self, _: &str) -> Result<Option<crate::Delivery>> {
            Ok(None)
        }
        async fn ack(&self, _: &crate::DeliveryId) -> Result<()> {
            Ok(())
        }
        async fn nack(&self, _: &crate::DeliveryId, _: crate::Nack) -> Result<()> {
            Ok(())
        }
        async fn depth(&self, _: &str) -> Result<usize> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn published_message_is_marked_in_the_outbox() {
        let (d, _, store) = make_dispatcher();
        d.dispatch(sample_request()).await.unwrap();
        assert!(store.pending_outbox(i64::MAX, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_publication_keeps_the_run_and_its_message_for_the_relay() {
        let store: Arc<InMemoryRunStateStore> = Arc::new(InMemoryRunStateStore::new());
        let d = QueueRunDispatcher::new(
            Arc::new(DownQueue),
            Arc::clone(&store) as Arc<dyn RunStateStore + Send + Sync>,
        )
        .with_id_generator(|| "r-down".into());
        // Le run est créé : la publication sera reprise par le relais.
        let outcome = d.dispatch(sample_request()).await.unwrap();
        assert_eq!(outcome.run_id, "r-down");
        assert_eq!(
            store.load("r-down").await.unwrap().unwrap().status,
            RunStatus::Queued
        );
        let pending = store.pending_outbox(i64::MAX, 10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].queue, "runs");
        assert_eq!(pending[0].generation, 0);
        assert_eq!(pending[0].message.body["run_id"], "r-down");
        assert_eq!(
            pending[0].message.body["payload"],
            json!({ "input": "hello" })
        );
        assert_eq!(
            pending[0]
                .message
                .headers
                .get("idempotency-key")
                .map(String::as_str),
            Some("r-down")
        );
    }

    #[tokio::test]
    async fn dispatch_refuses_an_existing_run_id() {
        let (d, queue, _) = make_dispatcher();
        d.dispatch(sample_request()).await.unwrap();
        assert!(d.dispatch(sample_request()).await.is_err());
        assert_eq!(queue.depth("runs").await.unwrap(), 1, "rien de republié");
    }

    #[tokio::test]
    async fn dispatch_outcome_equals_run_id_in_store() {
        let (d, _, store) = make_dispatcher();
        let outcome = d.dispatch(sample_request()).await.unwrap();
        let snap = store.load(&outcome.run_id).await.unwrap().unwrap();
        assert_eq!(snap.run_id, outcome.run_id);
    }
}

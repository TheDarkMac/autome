//! Déclenchement d'un run (F7.2.a).
//!
//! Abstrait l'opération « démarrer un workflow » derrière un trait
//! [`RunDispatcher`] consommable par l'API publique (F7.2.b). L'impl
//! [`QueueRunDispatcher`] est la version « production » : elle persiste un
//! `RunCheckpoint { status: Queued }` dans le [`RunStateStore`] et publie
//! un `Message { source: "manual" }` sur la [`Queue`] que le worker pool
//! consommera (F5.2). Les tests des handlers HTTP utilisent un mock pour
//! observer les appels sans dépendre du chemin queue + worker.
//!
//! Le contrat est volontairement étroit (`dispatch` renvoie le `run_id`
//! généré, l'API reste agnostique du format d'identifiant — string en
//! attendant les UUID PG).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::queue::{Message, Queue};
use crate::run_state::{RunCheckpoint, RunStateStore};
use crate::{Error, Result};

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
    /// l'état initial (Queued) puis publie un message dispatching.
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

        // 1. Persistance de l'état initial (Queued).
        let checkpoint = RunCheckpoint::queued(
            &run_id,
            &req.workflow_id,
            req.version,
            &req.trigger_type,
            req.now_ms,
        )
        .with_owner(req.owner_id);
        self.store.checkpoint(&checkpoint).await?;

        // 2. Publication sur la queue (le worker la consommera).
        let body = json!({
            "run_id": &run_id,
            "workflow_id": &req.workflow_id,
            "version": req.version,
            "trigger_type": &req.trigger_type,
            "payload": req.payload,
        });
        let msg = Message::new(body)
            .with_header("idempotency-key", &run_id)
            .with_header("trigger-source", &req.trigger_type);
        self.queue
            .publish(&self.queue_name, msg)
            .await
            .map_err(|e| {
                // Si la publication échoue, le run reste à `Queued` dans le store
                // — un sweep d'orphelins (F5.4.c) finira par le marquer Failed.
                // On propage l'erreur telle quelle au caller HTTP.
                Error::Invalid(format!("dispatch : publication échouée : {e}"))
            })?;

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

    #[tokio::test]
    async fn dispatch_outcome_equals_run_id_in_store() {
        let (d, _, store) = make_dispatcher();
        let outcome = d.dispatch(sample_request()).await.unwrap();
        let snap = store.load(&outcome.run_id).await.unwrap().unwrap();
        assert_eq!(snap.run_id, outcome.run_id);
    }
}

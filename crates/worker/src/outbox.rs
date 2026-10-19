//! Outbox du dispatch et remise en file des runs interrompus, côté worker
//! (#239).
//!
//! - **Relais** ([`relay_outbox`]) : le dispatcher (API, déclencheurs) écrit le
//!   run et son message ensemble puis publie lui-même ; si la publication a
//!   échoué (broker coupé, process tué entre les deux), le message reste « à
//!   publier » et le relais le publie. Publication *au moins une fois* : un
//!   doublon est écarté par le claim du run (#174).
//! - **Runs sans battement** ([`requeue_stale`]) : un run `running` dont le
//!   worker ne bat plus depuis le bail ([`lease_from_env`]) — worker tué,
//!   coupé du réseau — est remis en file et repris par un autre worker à son
//!   point de reprise. La tentative (fencing) empêche l'ancien worker, s'il
//!   revient, d'écrire quoi que ce soit.
//!
//! Chaque réplique fait tourner les deux, toutes les
//! [`MAINTENANCE_INTERVAL`] : sans élection de leader, les remises en file
//! sont gardées par la tentative et un message publié deux fois est écarté.

use std::time::Duration;

use autome_core::{OutboxMessage, Queue, RunStateStore};
use tracing::{info, warn};

/// Période du relais de l'outbox et de la remise en file des runs sans
/// battement.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5);

/// Âge minimal d'un message avant que le relais le publie : laisse au
/// dispatcher le temps de le faire lui-même (pas de doublon systématique).
pub const RELAY_GRACE: Duration = Duration::from_secs(5);

/// Messages publiés au plus par passe du relais.
const RELAY_BATCH: usize = 100;

/// Bail par défaut d'un run : sans battement depuis ce délai, il est remis en
/// file (le worker bat tous les quarts de bail).
pub const DEFAULT_LEASE: Duration = Duration::from_secs(60);

/// Plancher du bail : en dessous, une pause du worker (GC du broker, base
/// lente) suffirait à faire reprendre un run encore en cours.
const MIN_LEASE: Duration = Duration::from_secs(10);

/// `AUTOME_RUN_LEASE_SECS` (secondes, défaut 60, plancher 10).
pub fn lease_from_env() -> Result<Duration, String> {
    lease_from(std::env::var("AUTOME_RUN_LEASE_SECS").ok())
}

fn lease_from(value: Option<String>) -> Result<Duration, String> {
    match value {
        None => Ok(DEFAULT_LEASE),
        Some(v) if v.trim().is_empty() => Ok(DEFAULT_LEASE),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map(|s| Duration::from_secs(s).max(MIN_LEASE))
            .map_err(|_| {
                format!("AUTOME_RUN_LEASE_SECS : nombre de secondes attendu, obtenu `{v}`")
            }),
    }
}

/// Publie `message` sur sa queue puis le marque publié. `false` si la
/// publication échoue : le message reste à publier, le relais le reprendra.
pub async fn publish(
    queue: &dyn Queue,
    store: &dyn RunStateStore,
    message: &OutboxMessage,
    now_ms: i64,
) -> bool {
    if let Err(e) = queue.publish(&message.queue, message.message.clone()).await {
        warn!(
            run_id = %message.run_id,
            error = %e,
            "publication différée : le relais de l'outbox la reprendra"
        );
        return false;
    }
    if let Err(e) = store
        .mark_published(&message.run_id, message.generation, now_ms)
        .await
    {
        // Republié par le relais : doublon écarté par le claim.
        warn!(run_id = %message.run_id, error = %e, "publication non marquée dans l'outbox");
    }
    true
}

/// Publie les messages restés en attente depuis plus de [`RELAY_GRACE`] et
/// renvoie leur nombre. S'arrête à la première publication en échec (broker
/// coupé : inutile d'insister sur le reste du lot).
pub async fn relay_outbox(
    queue: &dyn Queue,
    store: &dyn RunStateStore,
    now_ms: i64,
) -> autome_core::Result<usize> {
    let grace = RELAY_GRACE.as_millis() as i64;
    let pending = store
        .pending_outbox(now_ms.saturating_sub(grace), RELAY_BATCH)
        .await?;
    let mut published = 0;
    for message in &pending {
        if !publish(queue, store, message, now_ms).await {
            return Err(autome_core::Error::Invalid(format!(
                "relais de l'outbox : publication impossible ({} message(s) publié(s) sur {})",
                published,
                pending.len()
            )));
        }
        info!(run_id = %message.run_id, generation = message.generation, "message de run publié par le relais");
        published += 1;
    }
    Ok(published)
}

/// Remet en file les runs `running` sans battement depuis `lease_ms` et
/// publie leur message ; renvoie leur nombre. Un run sans message (antérieur
/// à l'outbox) est laissé au balayage des orphelins.
pub async fn requeue_stale(
    queue: &dyn Queue,
    store: &dyn RunStateStore,
    now_ms: i64,
    lease_ms: i64,
) -> autome_core::Result<usize> {
    let mut requeued = 0;
    for run in store.list_stale(now_ms.saturating_sub(lease_ms)).await? {
        // Gardé par la tentative lue : sans effet si le run a battu, fini ou
        // été repris entre-temps.
        let Some(message) = store.requeue(&run.run_id, run.attempt, now_ms).await? else {
            continue;
        };
        warn!(
            run_id = %run.run_id,
            attempt = run.attempt,
            next_node = run.next_node.as_deref(),
            "run sans battement remis en file (worker arrêté ?)"
        );
        publish(queue, store, &message, now_ms).await;
        requeued += 1;
    }
    Ok(requeued)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{
        outbox_idempotency_key, Delivery, DeliveryId, InMemoryQueue, InMemoryRunStateStore,
        Message, Nack, RunCheckpoint, RunStatus,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    const NOW: i64 = 1_700_000_000_000;

    fn message(run_id: &str) -> OutboxMessage {
        OutboxMessage {
            run_id: run_id.into(),
            queue: "runs".into(),
            generation: 0,
            message: Message::new(json!({ "run_id": run_id }))
                .with_header("idempotency-key", outbox_idempotency_key(run_id, 0)),
        }
    }

    async fn enqueue(store: &InMemoryRunStateStore, run_id: &str, created_at_ms: i64) {
        store
            .enqueue(
                &RunCheckpoint::queued(run_id, "wf", 1, "manual", created_at_ms),
                &message(run_id),
            )
            .await
            .unwrap();
    }

    /// File en mémoire dont la publication peut être coupée.
    #[derive(Default)]
    struct SwitchQueue {
        inner: InMemoryQueue,
        down: AtomicBool,
    }

    #[async_trait::async_trait]
    impl Queue for SwitchQueue {
        async fn publish(&self, name: &str, msg: Message) -> autome_core::Result<()> {
            if self.down.load(Ordering::SeqCst) {
                return Err(autome_core::Error::Invalid("broker injoignable".into()));
            }
            self.inner.publish(name, msg).await
        }
        async fn poll(&self, name: &str) -> autome_core::Result<Option<Delivery>> {
            self.inner.poll(name).await
        }
        async fn ack(&self, id: &DeliveryId) -> autome_core::Result<()> {
            self.inner.ack(id).await
        }
        async fn nack(&self, id: &DeliveryId, verdict: Nack) -> autome_core::Result<()> {
            self.inner.nack(id, verdict).await
        }
        async fn depth(&self, name: &str) -> autome_core::Result<usize> {
            self.inner.depth(name).await
        }
    }

    #[test]
    fn lease_configuration() {
        assert_eq!(lease_from(None).unwrap(), DEFAULT_LEASE);
        assert_eq!(lease_from(Some(" ".into())).unwrap(), DEFAULT_LEASE);
        assert_eq!(
            lease_from(Some("120".into())).unwrap(),
            Duration::from_secs(120)
        );
        assert_eq!(lease_from(Some("1".into())).unwrap(), MIN_LEASE, "plancher");
        let err = lease_from(Some("1m".into())).unwrap_err();
        assert!(err.contains("AUTOME_RUN_LEASE_SECS"), "{err}");
    }

    #[tokio::test]
    async fn relay_publishes_only_messages_past_the_grace_period() {
        let store = InMemoryRunStateStore::new();
        let queue = SwitchQueue::default();
        let grace = RELAY_GRACE.as_millis() as i64;
        enqueue(&store, "old", NOW - grace - 1).await;
        enqueue(&store, "young", NOW - 1).await;

        assert_eq!(relay_outbox(&queue, &store, NOW).await.unwrap(), 1);
        let delivery = queue.poll("runs").await.unwrap().expect("publié");
        assert_eq!(delivery.message.body["run_id"], "old");
        assert!(queue.poll("runs").await.unwrap().is_none());
        // Publié : plus rien à relayer pour lui.
        assert_eq!(relay_outbox(&queue, &store, NOW).await.unwrap(), 0);
        // Le plus jeune passe une fois le délai écoulé.
        assert_eq!(relay_outbox(&queue, &store, NOW + grace).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn relay_keeps_messages_while_the_broker_is_down() {
        let store = InMemoryRunStateStore::new();
        let queue = SwitchQueue::default();
        enqueue(&store, "r", 1).await;
        queue.down.store(true, Ordering::SeqCst);
        assert!(relay_outbox(&queue, &store, NOW).await.is_err());
        assert_eq!(store.pending_outbox(i64::MAX, 10).await.unwrap().len(), 1);
        queue.down.store(false, Ordering::SeqCst);
        assert_eq!(relay_outbox(&queue, &store, NOW).await.unwrap(), 1);
        assert_eq!(queue.depth("runs").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn stale_runs_are_requeued_and_republished() {
        let store = InMemoryRunStateStore::new();
        let queue = SwitchQueue::default();
        let lease = DEFAULT_LEASE.as_millis() as i64;
        for id in ["dead", "alive"] {
            enqueue(&store, id, 1).await;
            store.mark_published(id, 0, 2).await.unwrap();
            store.try_claim(id, NOW - lease - 1).await.unwrap().unwrap();
        }
        assert!(store.heartbeat("alive", 1, NOW - 1).await.unwrap());

        assert_eq!(requeue_stale(&queue, &store, NOW, lease).await.unwrap(), 1);
        assert_eq!(
            store.load("dead").await.unwrap().unwrap().status,
            RunStatus::Queued
        );
        assert_eq!(
            store.load("alive").await.unwrap().unwrap().status,
            RunStatus::Running
        );
        let delivery = queue.poll("runs").await.unwrap().expect("republié");
        assert_eq!(delivery.message.body["run_id"], "dead");
        assert_eq!(
            delivery
                .message
                .headers
                .get("idempotency-key")
                .map(String::as_str),
            Some("dead#1")
        );
        assert!(store.pending_outbox(i64::MAX, 10).await.unwrap().is_empty());
        // Déjà remis en file : rien de plus au passage suivant.
        assert_eq!(requeue_stale(&queue, &store, NOW, lease).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_requeued_run_survives_a_broker_outage() {
        let store = InMemoryRunStateStore::new();
        let queue = SwitchQueue::default();
        enqueue(&store, "r", 1).await;
        store.mark_published("r", 0, 2).await.unwrap();
        store.try_claim("r", 10).await.unwrap().unwrap();
        queue.down.store(true, Ordering::SeqCst);
        assert_eq!(requeue_stale(&queue, &store, NOW, 1_000).await.unwrap(), 1);
        // Message resté à publier : le relais le publiera au retour du broker.
        queue.down.store(false, Ordering::SeqCst);
        assert_eq!(
            relay_outbox(&queue, &store, NOW + RELAY_GRACE.as_millis() as i64 + 1)
                .await
                .unwrap(),
            1
        );
        assert_eq!(queue.depth("runs").await.unwrap(), 1);
    }
}

//! File de messages (F5.1).
//!
//! Pose le contrat *agnostique du broker* sur lequel reposeront le worker
//! pool (F5.2), le planificateur (F5.3) et la persistance d'état (F5.4) :
//!
//! - [`Queue`] : `publish` / `poll` / `ack` / `nack` / `depth` — minimal,
//!   typé sans dépendance réseau ;
//! - [`Message`] : payload (`Value`) + en-têtes libres ;
//! - [`Delivery`] : message en main du consommateur, doit être `ack` ou
//!   `nack` (ack par message — sémantique task-queue) ;
//! - [`Nack`] : verdict d'échec (`Requeue` pour retry, `DeadLetter` pour
//!   isolement) ;
//! - [`InMemoryQueue`] : implémentation de référence, FIFO thread-safe,
//!   utilisée en tests et en mode dev. Le client AMQP réel (`lapin`) sera
//!   branché avec F5.2 — cf. [ADR 0003](../../../docs/adr/0003-choix-broker-messages.md).
//!
//! ## DLQ (F5.1.d)
//!
//! Convention : pour une queue `q`, la DLQ associée est `dlq_of(q)` =
//! `"<q>.dlq"`. `Nack::DeadLetter` republie le message vers la DLQ en
//! préservant le compteur `redeliveries`. Le retry classique
//! (`Nack::Requeue`) remet le message en tête de la queue principale et
//! incrémente `redeliveries` à la prochaine livraison.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::Value;

use crate::{Error, Result};

/// Identifiant unique d'une livraison (acknowledge token).
///
/// Stable pour un même `poll()` ; non-réutilisé tant que la livraison
/// n'a pas été acquittée ou ré-enqueuée.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeliveryId(u64);

impl DeliveryId {
    /// Construit un `DeliveryId` à partir d'une valeur brute (typiquement
    /// renvoyée par l'impl concrète du broker).
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }
    /// Valeur brute (utile pour logs / debug).
    pub fn raw(&self) -> u64 {
        self.0
    }
}

/// Message publié sur une queue.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// En-têtes libres (priorité AMQP, idempotency-key, trace-id, …).
    pub headers: BTreeMap<String, String>,
    /// Payload JSON (généralement le descripteur d'un run).
    pub body: Value,
}

impl Message {
    /// Construit un message sans en-tête à partir d'un `body` JSON.
    pub fn new(body: Value) -> Self {
        Self {
            headers: BTreeMap::new(),
            body,
        }
    }

    /// Ajoute un en-tête (builder).
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }
}

/// Livraison reçue d'un appel à `poll`.
///
/// L'appelant **doit** appeler [`Queue::ack`] ou [`Queue::nack`] après
/// traitement, sinon le message reste en in-flight indéfiniment (côté
/// impl de référence) ou expire suivant la politique du broker réel.
#[derive(Debug, Clone)]
pub struct Delivery {
    /// Token d'acknowledgement.
    pub id: DeliveryId,
    /// Queue d'origine.
    pub queue: String,
    /// Message livré.
    pub message: Message,
    /// Nombre de fois où ce message a déjà été livré (0 pour la 1ʳᵉ).
    pub redeliveries: u32,
}

/// Verdict d'échec après un `poll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nack {
    /// Remet le message en tête de la queue d'origine (retry — `redeliveries`++).
    Requeue,
    /// Déplace le message vers la DLQ associée — pas de retry supplémentaire.
    DeadLetter,
}

/// Contrat racine d'un broker de messages.
///
/// Volontairement *sync* dans cette première itération pour rester cohérent
/// avec le reste de la stack ; l'API publique exposera un wrapper async
/// (`tokio::task::spawn_blocking` ou un client `lapin` async direct) en F5.2.
pub trait Queue {
    /// Publie `msg` sur la queue `name`. La queue est créée à la volée
    /// (côté impl de référence) ou doit être déclarée en amont (RabbitMQ).
    fn publish(&self, name: &str, msg: Message) -> Result<()>;

    /// Récupère le prochain message de la queue `name` (FIFO).
    /// Retourne `None` si la queue est vide.
    ///
    /// Le message passe en *in-flight* jusqu'à `ack`/`nack`.
    fn poll(&self, name: &str) -> Result<Option<Delivery>>;

    /// Confirme le succès du traitement — supprime le message in-flight.
    fn ack(&self, id: &DeliveryId) -> Result<()>;

    /// Reporte l'échec — selon le verdict, requeue ou déplace en DLQ.
    fn nack(&self, id: &DeliveryId, verdict: Nack) -> Result<()>;

    /// Nombre de messages **en attente** (hors in-flight) sur `name`.
    fn depth(&self, name: &str) -> Result<usize>;
}

/// Convention de nommage des dead-letter queues.
///
/// La DLQ de `q` est `"{q}.dlq"`. Centralisé ici pour qu'aucun appelant
/// n'invente sa propre convention.
pub fn dlq_of(queue: &str) -> String {
    format!("{queue}.dlq")
}

// ────────────────────── InMemoryQueue ──────────────────────

#[derive(Debug, Clone)]
struct Pending {
    queue: String,
    message: Message,
    redeliveries: u32,
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    queues: HashMap<String, VecDeque<Pending>>,
    in_flight: HashMap<DeliveryId, Pending>,
}

/// Implémentation de référence, in-process, thread-safe (`Mutex`).
///
/// FIFO par queue, queues isolées par nom. La DLQ d'une queue `q` est
/// elle-même une queue interne de nom `dlq_of(q)` — interrogeable comme
/// une autre via [`Queue::poll`] / [`Queue::depth`].
#[derive(Debug, Default)]
pub struct InMemoryQueue {
    state: Mutex<State>,
}

impl InMemoryQueue {
    /// Crée une queue vide.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Queue for InMemoryQueue {
    fn publish(&self, name: &str, msg: Message) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Invalid("queue lock empoisonné".into()))?;
        state
            .queues
            .entry(name.to_string())
            .or_default()
            .push_back(Pending {
                queue: name.to_string(),
                message: msg,
                redeliveries: 0,
            });
        Ok(())
    }

    fn poll(&self, name: &str) -> Result<Option<Delivery>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Invalid("queue lock empoisonné".into()))?;
        let Some(pending) = state.queues.get_mut(name).and_then(VecDeque::pop_front) else {
            return Ok(None);
        };
        state.next_id += 1;
        let id = DeliveryId(state.next_id);
        let delivery = Delivery {
            id: id.clone(),
            queue: pending.queue.clone(),
            message: pending.message.clone(),
            redeliveries: pending.redeliveries,
        };
        state.in_flight.insert(id, pending);
        Ok(Some(delivery))
    }

    fn ack(&self, id: &DeliveryId) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Invalid("queue lock empoisonné".into()))?;
        state
            .in_flight
            .remove(id)
            .ok_or_else(|| Error::Invalid(format!("ack : delivery {} inconnue", id.raw())))?;
        Ok(())
    }

    fn nack(&self, id: &DeliveryId, verdict: Nack) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Invalid("queue lock empoisonné".into()))?;
        let mut pending = state
            .in_flight
            .remove(id)
            .ok_or_else(|| Error::Invalid(format!("nack : delivery {} inconnue", id.raw())))?;
        match verdict {
            Nack::Requeue => {
                pending.redeliveries += 1;
                let queue = pending.queue.clone();
                state.queues.entry(queue).or_default().push_front(pending);
            }
            Nack::DeadLetter => {
                let dlq = dlq_of(&pending.queue);
                let dlq_pending = Pending {
                    queue: dlq.clone(),
                    message: pending.message,
                    redeliveries: pending.redeliveries,
                };
                state.queues.entry(dlq).or_default().push_back(dlq_pending);
            }
        }
        Ok(())
    }

    fn depth(&self, name: &str) -> Result<usize> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::Invalid("queue lock empoisonné".into()))?;
        Ok(state.queues.get(name).map(VecDeque::len).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(body: Value) -> Message {
        Message::new(body)
    }

    #[test]
    fn publish_increments_depth() {
        let q = InMemoryQueue::new();
        assert_eq!(q.depth("runs").unwrap(), 0);
        q.publish("runs", msg(json!({ "n": 1 }))).unwrap();
        q.publish("runs", msg(json!({ "n": 2 }))).unwrap();
        assert_eq!(q.depth("runs").unwrap(), 2);
    }

    #[test]
    fn poll_returns_messages_in_fifo_order() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({ "n": 1 }))).unwrap();
        q.publish("runs", msg(json!({ "n": 2 }))).unwrap();
        q.publish("runs", msg(json!({ "n": 3 }))).unwrap();
        let d1 = q.poll("runs").unwrap().unwrap();
        let d2 = q.poll("runs").unwrap().unwrap();
        let d3 = q.poll("runs").unwrap().unwrap();
        assert_eq!(d1.message.body, json!({ "n": 1 }));
        assert_eq!(d2.message.body, json!({ "n": 2 }));
        assert_eq!(d3.message.body, json!({ "n": 3 }));
        assert_eq!(d1.queue, "runs");
        assert_eq!(d1.redeliveries, 0);
    }

    #[test]
    fn poll_on_empty_queue_returns_none() {
        let q = InMemoryQueue::new();
        assert!(q.poll("missing").unwrap().is_none());
    }

    #[test]
    fn poll_moves_message_to_in_flight() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({}))).unwrap();
        assert_eq!(q.depth("runs").unwrap(), 1);
        let _d = q.poll("runs").unwrap().unwrap();
        // Plus dans la file principale (mais en in-flight) :
        assert_eq!(q.depth("runs").unwrap(), 0);
    }

    #[test]
    fn ack_removes_in_flight() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({}))).unwrap();
        let d = q.poll("runs").unwrap().unwrap();
        q.ack(&d.id).unwrap();
        // Tenter d'ack une seconde fois échoue : delivery déjà acquittée.
        let err = q.ack(&d.id).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("delivery")));
    }

    #[test]
    fn nack_requeue_puts_message_back_with_redelivery_count() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({ "n": 1 }))).unwrap();
        let d1 = q.poll("runs").unwrap().unwrap();
        assert_eq!(d1.redeliveries, 0);
        q.nack(&d1.id, Nack::Requeue).unwrap();
        // Le message est de retour en tête de file
        assert_eq!(q.depth("runs").unwrap(), 1);
        let d2 = q.poll("runs").unwrap().unwrap();
        assert_eq!(d2.message.body, json!({ "n": 1 }));
        assert_eq!(d2.redeliveries, 1);
        // Re-nack, re-poll : compteur = 2
        q.nack(&d2.id, Nack::Requeue).unwrap();
        let d3 = q.poll("runs").unwrap().unwrap();
        assert_eq!(d3.redeliveries, 2);
    }

    #[test]
    fn nack_dead_letter_routes_to_dlq() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({ "n": 7 }))).unwrap();
        let d = q.poll("runs").unwrap().unwrap();
        q.nack(&d.id, Nack::DeadLetter).unwrap();
        // Queue principale vide :
        assert_eq!(q.depth("runs").unwrap(), 0);
        // DLQ peuplée :
        assert_eq!(q.depth("runs.dlq").unwrap(), 1);
        let dead = q.poll("runs.dlq").unwrap().unwrap();
        assert_eq!(dead.queue, "runs.dlq");
        assert_eq!(dead.message.body, json!({ "n": 7 }));
    }

    #[test]
    fn dlq_preserves_redelivery_count() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({}))).unwrap();
        // 3 requeues puis DLQ
        for _ in 0..3 {
            let d = q.poll("runs").unwrap().unwrap();
            q.nack(&d.id, Nack::Requeue).unwrap();
        }
        let d = q.poll("runs").unwrap().unwrap();
        assert_eq!(d.redeliveries, 3);
        q.nack(&d.id, Nack::DeadLetter).unwrap();
        let dead = q.poll("runs.dlq").unwrap().unwrap();
        assert_eq!(dead.redeliveries, 3);
    }

    #[test]
    fn queues_are_isolated_by_name() {
        let q = InMemoryQueue::new();
        q.publish("a", msg(json!({ "src": "a" }))).unwrap();
        q.publish("b", msg(json!({ "src": "b" }))).unwrap();
        let da = q.poll("a").unwrap().unwrap();
        let db = q.poll("b").unwrap().unwrap();
        assert_eq!(da.message.body, json!({ "src": "a" }));
        assert_eq!(db.message.body, json!({ "src": "b" }));
        // « a » est vide, mais pas « b » s'il y avait eu plus
        assert!(q.poll("a").unwrap().is_none());
    }

    #[test]
    fn ack_unknown_delivery_errors() {
        let q = InMemoryQueue::new();
        let err = q.ack(&DeliveryId::new(999)).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("999")));
    }

    #[test]
    fn nack_unknown_delivery_errors() {
        let q = InMemoryQueue::new();
        let err = q.nack(&DeliveryId::new(123), Nack::Requeue).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("123")));
    }

    #[test]
    fn message_headers_round_trip() {
        let q = InMemoryQueue::new();
        let m = Message::new(json!({ "n": 1 }))
            .with_header("priority", "5")
            .with_header("trace-id", "abc");
        q.publish("runs", m.clone()).unwrap();
        let d = q.poll("runs").unwrap().unwrap();
        assert_eq!(
            d.message.headers.get("priority").map(String::as_str),
            Some("5")
        );
        assert_eq!(
            d.message.headers.get("trace-id").map(String::as_str),
            Some("abc")
        );
        assert_eq!(d.message, m);
    }

    #[test]
    fn dlq_of_uses_dlq_suffix() {
        assert_eq!(dlq_of("runs"), "runs.dlq");
        assert_eq!(dlq_of("workflows.events"), "workflows.events.dlq");
    }

    #[test]
    fn delivery_id_is_unique_per_poll() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({}))).unwrap();
        q.publish("runs", msg(json!({}))).unwrap();
        let d1 = q.poll("runs").unwrap().unwrap();
        let d2 = q.poll("runs").unwrap().unwrap();
        assert_ne!(d1.id, d2.id);
    }

    /// Vérifie la composition : Send + Sync sur l'impl de référence
    /// (utilisable depuis plusieurs threads — précondition pour le worker
    /// pool F5.2).
    #[test]
    fn in_memory_queue_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryQueue>();
    }
}

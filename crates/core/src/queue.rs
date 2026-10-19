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

use async_trait::async_trait;
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
/// **Async** (dette #107) : l'impl réelle (`lapin`/AMQP) fait des I/O réseau ;
/// le worker async la consomme directement. L'impl in-memory reste triviale.
#[async_trait]
pub trait Queue: Send + Sync {
    /// Publie `msg` sur la queue `name`. La queue est créée à la volée
    /// (côté impl de référence) ou doit être déclarée en amont (RabbitMQ).
    async fn publish(&self, name: &str, msg: Message) -> Result<()>;

    /// Récupère le prochain message de la queue `name` (FIFO).
    /// Retourne `None` si la queue est vide.
    ///
    /// Le message passe en *in-flight* jusqu'à `ack`/`nack`.
    async fn poll(&self, name: &str) -> Result<Option<Delivery>>;

    /// Attend **au plus** `wait` le prochain message de `name` (consommation
    /// continue, #237) ; `None` si rien n'est arrivé dans ce délai.
    ///
    /// Même contrat d'acquittement que [`poll`](Self::poll). Une impl peut
    /// rendre la main avant `wait` sans message (l'appelant qui boucle doit
    /// alors patienter le reste du délai). Par défaut : un seul `poll`, sans
    /// attente. RabbitMQ la sert par un consommateur `basic_consume` dont le
    /// broker pousse les messages, au lieu d'un aller-retour par message.
    async fn receive(&self, name: &str, wait: std::time::Duration) -> Result<Option<Delivery>> {
        let _ = wait;
        self.poll(name).await
    }

    /// Met fin à la consommation continue de `name` par ce process (arrêt du
    /// worker) : les messages reçus d'avance et pas encore rendus par
    /// [`receive`](Self::receive) retournent en file pour les autres
    /// consommateurs. Les livraisons déjà rendues restent à acquitter. Un
    /// `receive` ultérieur sur `name` peut ne plus rien livrer. Par défaut :
    /// rien à faire.
    async fn stop_receiving(&self, name: &str) -> Result<()> {
        let _ = name;
        Ok(())
    }

    /// Confirme le succès du traitement — supprime le message in-flight.
    async fn ack(&self, id: &DeliveryId) -> Result<()>;

    /// Reporte l'échec — selon le verdict, requeue ou déplace en DLQ.
    async fn nack(&self, id: &DeliveryId, verdict: Nack) -> Result<()>;

    /// Nombre de messages **en attente** (hors in-flight) sur `name`.
    async fn depth(&self, name: &str) -> Result<usize>;

    /// Vérifie que le broker est joignable (sonde de disponibilité, #192) ;
    /// une impl distante rouvre au besoin sa connexion. Toujours `Ok` pour
    /// une file en mémoire.
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
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

#[async_trait]
impl Queue for InMemoryQueue {
    async fn publish(&self, name: &str, msg: Message) -> Result<()> {
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

    async fn poll(&self, name: &str) -> Result<Option<Delivery>> {
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

    async fn ack(&self, id: &DeliveryId) -> Result<()> {
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

    async fn nack(&self, id: &DeliveryId, verdict: Nack) -> Result<()> {
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

    async fn depth(&self, name: &str) -> Result<usize> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::Invalid("queue lock empoisonné".into()))?;
        Ok(state.queues.get(name).map(VecDeque::len).unwrap_or(0))
    }
}

// ────────────────────── BoundedQueue (F5.5) ──────────────────────

/// Wrapper qui plafonne la profondeur d'une queue à `publish` — applique
/// la *backpressure* sur les producteurs (scheduler, API) quand les
/// consommateurs (worker pool) n'arrivent plus à suivre.
///
/// Construction builder :
/// ```ignore
/// let q = BoundedQueue::new(InMemoryQueue::new())
///     .with_default_limit(10_000)
///     .with_limit("runs", 1_000)
///     .with_limit("runs.dlq", 100);
/// ```
///
/// Sémantique de bound *soft* : la vérification se fait via
/// `inner.depth(name).await` puis `inner.publish(name, msg).await` — un autre
/// producteur peut s'intercaler. En pratique le dépassement est borné
/// par le nombre de publishers concurrents, ce qui est acceptable pour
/// un seuil de protection (pas un quota strict).
///
/// Les opérations `poll` / `ack` / `nack` / `depth` sont des
/// pass-through pour préserver le contrat de [`Queue`].
pub struct BoundedQueue<Q: Queue> {
    inner: Q,
    limits: HashMap<String, usize>,
    default_limit: Option<usize>,
}

impl<Q: Queue> BoundedQueue<Q> {
    /// Construit le wrapper. Sans aucun `with_limit` ni `with_default_limit`,
    /// se comporte comme `inner` (aucune borne — pass-through pur).
    pub fn new(inner: Q) -> Self {
        Self {
            inner,
            limits: HashMap::new(),
            default_limit: None,
        }
    }

    /// Builder : fixe la limite **par queue** (override le default).
    pub fn with_limit(mut self, name: impl Into<String>, max: usize) -> Self {
        self.limits.insert(name.into(), max);
        self
    }

    /// Builder : fixe la limite par défaut, appliquée aux queues sans
    /// limite explicite.
    pub fn with_default_limit(mut self, max: usize) -> Self {
        self.default_limit = Some(max);
        self
    }

    /// Limite effective pour `name` (clé explicite, sinon default, sinon `None`).
    pub fn limit_for(&self, name: &str) -> Option<usize> {
        self.limits.get(name).copied().or(self.default_limit)
    }

    /// Référence vers la queue wrappée (utile pour les tests).
    pub fn inner(&self) -> &Q {
        &self.inner
    }
}

#[async_trait]
impl<Q: Queue> Queue for BoundedQueue<Q> {
    async fn publish(&self, name: &str, msg: Message) -> Result<()> {
        if let Some(max) = self.limit_for(name) {
            let depth = self.inner.depth(name).await?;
            if depth >= max {
                return Err(Error::Backpressure {
                    queue: name.to_string(),
                    depth,
                    max,
                });
            }
        }
        self.inner.publish(name, msg).await
    }

    async fn poll(&self, name: &str) -> Result<Option<Delivery>> {
        self.inner.poll(name).await
    }

    async fn receive(&self, name: &str, wait: std::time::Duration) -> Result<Option<Delivery>> {
        self.inner.receive(name, wait).await
    }

    async fn stop_receiving(&self, name: &str) -> Result<()> {
        self.inner.stop_receiving(name).await
    }

    async fn ack(&self, id: &DeliveryId) -> Result<()> {
        self.inner.ack(id).await
    }

    async fn nack(&self, id: &DeliveryId, verdict: Nack) -> Result<()> {
        self.inner.nack(id, verdict).await
    }

    async fn depth(&self, name: &str) -> Result<usize> {
        self.inner.depth(name).await
    }

    async fn ping(&self) -> Result<()> {
        self.inner.ping().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(body: Value) -> Message {
        Message::new(body)
    }

    #[tokio::test]
    async fn publish_increments_depth() {
        let q = InMemoryQueue::new();
        assert_eq!(q.depth("runs").await.unwrap(), 0);
        q.publish("runs", msg(json!({ "n": 1 }))).await.unwrap();
        q.publish("runs", msg(json!({ "n": 2 }))).await.unwrap();
        assert_eq!(q.depth("runs").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn poll_returns_messages_in_fifo_order() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({ "n": 1 }))).await.unwrap();
        q.publish("runs", msg(json!({ "n": 2 }))).await.unwrap();
        q.publish("runs", msg(json!({ "n": 3 }))).await.unwrap();
        let d1 = q.poll("runs").await.unwrap().unwrap();
        let d2 = q.poll("runs").await.unwrap().unwrap();
        let d3 = q.poll("runs").await.unwrap().unwrap();
        assert_eq!(d1.message.body, json!({ "n": 1 }));
        assert_eq!(d2.message.body, json!({ "n": 2 }));
        assert_eq!(d3.message.body, json!({ "n": 3 }));
        assert_eq!(d1.queue, "runs");
        assert_eq!(d1.redeliveries, 0);
    }

    #[tokio::test]
    async fn poll_on_empty_queue_returns_none() {
        let q = InMemoryQueue::new();
        assert!(q.poll("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn poll_moves_message_to_in_flight() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({}))).await.unwrap();
        assert_eq!(q.depth("runs").await.unwrap(), 1);
        let _d = q.poll("runs").await.unwrap().unwrap();
        // Plus dans la file principale (mais en in-flight) :
        assert_eq!(q.depth("runs").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn ack_removes_in_flight() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({}))).await.unwrap();
        let d = q.poll("runs").await.unwrap().unwrap();
        q.ack(&d.id).await.unwrap();
        // Tenter d'ack une seconde fois échoue : delivery déjà acquittée.
        let err = q.ack(&d.id).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("delivery")));
    }

    #[tokio::test]
    async fn nack_requeue_puts_message_back_with_redelivery_count() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({ "n": 1 }))).await.unwrap();
        let d1 = q.poll("runs").await.unwrap().unwrap();
        assert_eq!(d1.redeliveries, 0);
        q.nack(&d1.id, Nack::Requeue).await.unwrap();
        // Le message est de retour en tête de file
        assert_eq!(q.depth("runs").await.unwrap(), 1);
        let d2 = q.poll("runs").await.unwrap().unwrap();
        assert_eq!(d2.message.body, json!({ "n": 1 }));
        assert_eq!(d2.redeliveries, 1);
        // Re-nack, re-poll : compteur = 2
        q.nack(&d2.id, Nack::Requeue).await.unwrap();
        let d3 = q.poll("runs").await.unwrap().unwrap();
        assert_eq!(d3.redeliveries, 2);
    }

    #[tokio::test]
    async fn nack_dead_letter_routes_to_dlq() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({ "n": 7 }))).await.unwrap();
        let d = q.poll("runs").await.unwrap().unwrap();
        q.nack(&d.id, Nack::DeadLetter).await.unwrap();
        // Queue principale vide :
        assert_eq!(q.depth("runs").await.unwrap(), 0);
        // DLQ peuplée :
        assert_eq!(q.depth("runs.dlq").await.unwrap(), 1);
        let dead = q.poll("runs.dlq").await.unwrap().unwrap();
        assert_eq!(dead.queue, "runs.dlq");
        assert_eq!(dead.message.body, json!({ "n": 7 }));
    }

    #[tokio::test]
    async fn dlq_preserves_redelivery_count() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({}))).await.unwrap();
        // 3 requeues puis DLQ
        for _ in 0..3 {
            let d = q.poll("runs").await.unwrap().unwrap();
            q.nack(&d.id, Nack::Requeue).await.unwrap();
        }
        let d = q.poll("runs").await.unwrap().unwrap();
        assert_eq!(d.redeliveries, 3);
        q.nack(&d.id, Nack::DeadLetter).await.unwrap();
        let dead = q.poll("runs.dlq").await.unwrap().unwrap();
        assert_eq!(dead.redeliveries, 3);
    }

    #[tokio::test]
    async fn queues_are_isolated_by_name() {
        let q = InMemoryQueue::new();
        q.publish("a", msg(json!({ "src": "a" }))).await.unwrap();
        q.publish("b", msg(json!({ "src": "b" }))).await.unwrap();
        let da = q.poll("a").await.unwrap().unwrap();
        let db = q.poll("b").await.unwrap().unwrap();
        assert_eq!(da.message.body, json!({ "src": "a" }));
        assert_eq!(db.message.body, json!({ "src": "b" }));
        // « a » est vide, mais pas « b » s'il y avait eu plus
        assert!(q.poll("a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ack_unknown_delivery_errors() {
        let q = InMemoryQueue::new();
        let err = q.ack(&DeliveryId::new(999)).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("999")));
    }

    #[tokio::test]
    async fn nack_unknown_delivery_errors() {
        let q = InMemoryQueue::new();
        let err = q
            .nack(&DeliveryId::new(123), Nack::Requeue)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("123")));
    }

    #[tokio::test]
    async fn message_headers_round_trip() {
        let q = InMemoryQueue::new();
        let m = Message::new(json!({ "n": 1 }))
            .with_header("priority", "5")
            .with_header("trace-id", "abc");
        q.publish("runs", m.clone()).await.unwrap();
        let d = q.poll("runs").await.unwrap().unwrap();
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

    #[tokio::test]
    async fn dlq_of_uses_dlq_suffix() {
        assert_eq!(dlq_of("runs"), "runs.dlq");
        assert_eq!(dlq_of("workflows.events"), "workflows.events.dlq");
    }

    #[tokio::test]
    async fn delivery_id_is_unique_per_poll() {
        let q = InMemoryQueue::new();
        q.publish("runs", msg(json!({}))).await.unwrap();
        q.publish("runs", msg(json!({}))).await.unwrap();
        let d1 = q.poll("runs").await.unwrap().unwrap();
        let d2 = q.poll("runs").await.unwrap().unwrap();
        assert_ne!(d1.id, d2.id);
    }

    /// Vérifie la composition : Send + Sync sur l'impl de référence
    /// (utilisable depuis plusieurs threads — précondition pour le worker
    /// pool F5.2).
    #[tokio::test]
    async fn in_memory_queue_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryQueue>();
    }

    // ───── F5.5 — BoundedQueue ─────

    #[tokio::test]
    async fn bounded_without_limits_passes_through() {
        let q = BoundedQueue::new(InMemoryQueue::new());
        for i in 0..100 {
            q.publish("runs", msg(json!({ "n": i }))).await.unwrap();
        }
        assert_eq!(q.depth("runs").await.unwrap(), 100);
    }

    #[tokio::test]
    async fn bounded_rejects_when_limit_reached() {
        let q = BoundedQueue::new(InMemoryQueue::new()).with_limit("runs", 3);
        q.publish("runs", msg(json!({ "n": 1 }))).await.unwrap();
        q.publish("runs", msg(json!({ "n": 2 }))).await.unwrap();
        q.publish("runs", msg(json!({ "n": 3 }))).await.unwrap();
        let err = q.publish("runs", msg(json!({ "n": 4 }))).await.unwrap_err();
        match err {
            Error::Backpressure { queue, depth, max } => {
                assert_eq!(queue, "runs");
                assert_eq!(depth, 3);
                assert_eq!(max, 3);
            }
            other => panic!("expected Backpressure, got {other:?}"),
        }
        // Le message rejeté n'a pas été ajouté
        assert_eq!(q.depth("runs").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn bounded_allows_publish_after_poll_drains() {
        let q = BoundedQueue::new(InMemoryQueue::new()).with_limit("runs", 2);
        q.publish("runs", msg(json!({ "n": 1 }))).await.unwrap();
        q.publish("runs", msg(json!({ "n": 2 }))).await.unwrap();
        // À la limite → rejeté
        assert!(matches!(
            q.publish("runs", msg(json!({}))).await,
            Err(Error::Backpressure { .. })
        ));
        // Drain : poll récupère 1 message → depth=1, publish à nouveau OK
        let d = q.poll("runs").await.unwrap().unwrap();
        assert_eq!(q.depth("runs").await.unwrap(), 1);
        q.publish("runs", msg(json!({ "n": 3 }))).await.unwrap();
        assert_eq!(q.depth("runs").await.unwrap(), 2);
        // Ack ne change pas la depth (le message était déjà retiré de la
        // file principale par poll).
        q.ack(&d.id).await.unwrap();
        assert_eq!(q.depth("runs").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn bounded_per_queue_limits_are_isolated() {
        let q = BoundedQueue::new(InMemoryQueue::new())
            .with_limit("small", 1)
            .with_limit("large", 100);
        q.publish("small", msg(json!({}))).await.unwrap();
        assert!(matches!(
            q.publish("small", msg(json!({}))).await,
            Err(Error::Backpressure { .. })
        ));
        // Pas affecté
        for _ in 0..50 {
            q.publish("large", msg(json!({}))).await.unwrap();
        }
        assert_eq!(q.depth("large").await.unwrap(), 50);
    }

    #[tokio::test]
    async fn bounded_default_limit_covers_unspecified_queues() {
        let q = BoundedQueue::new(InMemoryQueue::new())
            .with_default_limit(2)
            .with_limit("special", 10);
        // Queue arbitraire → cap par défaut
        q.publish("a", msg(json!({}))).await.unwrap();
        q.publish("a", msg(json!({}))).await.unwrap();
        assert!(matches!(
            q.publish("a", msg(json!({}))).await,
            Err(Error::Backpressure { .. })
        ));
        // Queue avec override → cap explicite
        for _ in 0..10 {
            q.publish("special", msg(json!({}))).await.unwrap();
        }
        assert!(matches!(
            q.publish("special", msg(json!({}))).await,
            Err(Error::Backpressure { .. })
        ));
    }

    #[tokio::test]
    async fn bounded_explicit_limit_overrides_default() {
        let q = BoundedQueue::new(InMemoryQueue::new())
            .with_default_limit(100)
            .with_limit("tight", 1);
        q.publish("tight", msg(json!({}))).await.unwrap();
        assert!(matches!(
            q.publish("tight", msg(json!({}))).await,
            Err(Error::Backpressure { .. })
        ));
    }

    #[tokio::test]
    async fn bounded_nack_requeue_does_not_violate_limit() {
        let q = BoundedQueue::new(InMemoryQueue::new()).with_limit("runs", 1);
        q.publish("runs", msg(json!({ "n": 1 }))).await.unwrap();
        let d = q.poll("runs").await.unwrap().unwrap();
        // Après poll, depth=0 → on peut re-publish
        q.publish("runs", msg(json!({ "n": 2 }))).await.unwrap();
        assert_eq!(q.depth("runs").await.unwrap(), 1);
        // nack(Requeue) remet en tête → depth=2 (dépasse la limite, mais c'est
        // un nack interne, pas un publish — donc accepté).
        q.nack(&d.id, Nack::Requeue).await.unwrap();
        assert_eq!(q.depth("runs").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn bounded_limit_for_returns_effective_value() {
        let q = BoundedQueue::new(InMemoryQueue::new())
            .with_default_limit(10)
            .with_limit("explicit", 1);
        assert_eq!(q.limit_for("explicit"), Some(1));
        assert_eq!(q.limit_for("default"), Some(10));

        let q_none = BoundedQueue::new(InMemoryQueue::new());
        assert_eq!(q_none.limit_for("anything"), None);
    }

    #[tokio::test]
    async fn bounded_is_send_sync_when_inner_is() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BoundedQueue<InMemoryQueue>>();
    }

    #[tokio::test]
    async fn receive_defaults_to_a_single_poll_through_the_bounded_wrapper() {
        let q = BoundedQueue::new(InMemoryQueue::new()).with_limit("q", 10);
        let wait = std::time::Duration::from_secs(3600);
        // File vide : rend la main sans attendre le délai.
        assert!(q.receive("q", wait).await.unwrap().is_none());
        q.publish("q", msg(json!({ "n": 1 }))).await.unwrap();
        let d = q.receive("q", wait).await.unwrap().expect("un message");
        assert_eq!(d.message.body, json!({ "n": 1 }));
        q.ack(&d.id).await.unwrap();
        q.stop_receiving("q").await.unwrap();
        q.ping().await.unwrap();
    }

    #[tokio::test]
    async fn backpressure_error_message_is_explicit() {
        let q = BoundedQueue::new(InMemoryQueue::new()).with_limit("q", 1);
        q.publish("q", msg(json!({}))).await.unwrap();
        let err = q.publish("q", msg(json!({}))).await.unwrap_err();
        let display = format!("{err}");
        assert!(display.contains("backpressure"), "msg={display}");
        assert!(display.contains("q"), "msg={display}");
        assert!(display.contains("1/1"), "msg={display}");
    }
}

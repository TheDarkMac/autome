//! [`Queue`] adossée à **RabbitMQ** via `lapin` (AMQP async) — dette #107.
//!
//! Sémantique alignée sur `InMemoryQueue` :
//! - `publish` déclare la queue (durable) et publie sur l'exchange par défaut
//!   (routing key = nom de queue) ;
//! - `poll` fait un `basic_get` (un message à la fois), le message reste
//!   in-flight jusqu'à `ack`/`nack` ; le `DeliveryId` **est** le `delivery_tag`
//!   AMQP ;
//! - `receive` (le worker, #237) lit un **consommateur** `basic_consume` :
//!   le broker pousse les messages, au plus `prefetch` non acquittés à la
//!   fois ([`with_prefetch`](RabbitMqQueue::with_prefetch)), au lieu d'un
//!   aller-retour réseau par message et par tâche ;
//! - `nack(Requeue)` → `basic_nack(requeue=true)` ; `nack(DeadLetter)` →
//!   `basic_nack(requeue=false)` : la queue est déclarée avec un **dead-letter
//!   exchange** pointant la DLQ `dlq_of(name)`, donc RabbitMQ y route le message
//!   automatiquement ;
//! - `depth` lit le `message_count` d'une déclaration passive.
//!
//! ## Résilience (#180)
//!
//! - **Reconnexion** : si le canal est fermé (broker redémarré, réseau coupé),
//!   l'appel suivant rouvre connexion + canal (une tentative, bornée à
//!   [`OPEN_TIMEOUT`]) au lieu d'échouer indéfiniment.
//! - **Livraisons et génération** : un `delivery_tag` n'a de sens que sur le
//!   canal qui l'a émis. Le [`DeliveryId`] porte donc la génération du canal
//!   (16 bits de poids fort) ; un `ack`/`nack` d'une livraison d'un canal
//!   disparu est ignoré — le broker a déjà remis le message en file, et le
//!   claim atomique des runs (#174) rend sa redélivrance sûre.
//! - **Publisher confirms** : le canal est en mode confirm ; un `publish` ne
//!   réussit que si le broker a accepté le message (`nack` ⇒ erreur).
//! - **Déclarations en cache** (#237) : une queue (et sa DLQ) n'est déclarée
//!   qu'une fois par connexion. La publication est `mandatory` : un message
//!   non routé (queue supprimée depuis) est renvoyé par le broker, la queue
//!   redéclarée et le message republié au lieu d'être perdu en silence.
//! - **Consommateurs** : propres à un canal, recréés au premier `receive`
//!   après une reconnexion ; les messages reçus d'avance par un canal fermé
//!   sont remis en file par le broker.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use autome_core::{dlq_of, Delivery, DeliveryId, Error, Message, Nack, Queue};
use futures_lite::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicCancelOptions, BasicConsumeOptions, BasicGetOptions, BasicNackOptions,
    BasicPublishOptions, BasicQosOptions, ConfirmSelectOptions, ExchangeDeclareOptions,
    QueueDeclareOptions,
};
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties, Consumer, ExchangeKind};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

/// Délai maximal d'ouverture d'une connexion (évite de bloquer les appelants
/// indéfiniment sur un hôte qui ne répond pas).
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Messages non acquittés que le broker confie d'avance à un consommateur
/// (`basic_qos`), faute de [`RabbitMqQueue::with_prefetch`].
pub const DEFAULT_PREFETCH: u16 = 1;

/// Attente maximale d'un message reçu d'avance après l'annulation d'un
/// consommateur (`stop_receiving`) : ceux déjà en route arrivent avant
/// l'accusé d'annulation, ce délai ne borne qu'un broker muet.
const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Bits du `delivery_tag` dans un [`DeliveryId`] ; les 16 bits restants
/// portent la génération du canal.
const TAG_BITS: u32 = 48;
const TAG_MASK: u64 = (1 << TAG_BITS) - 1;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("rabbitmq: {context}: {e}"))
}

/// Encode `(génération du canal, delivery_tag)` dans un [`DeliveryId`].
fn encode_delivery(generation: u16, tag: u64) -> DeliveryId {
    debug_assert!(tag <= TAG_MASK, "delivery_tag hors des 48 bits");
    DeliveryId::new((u64::from(generation) << TAG_BITS) | (tag & TAG_MASK))
}

/// Décode un [`DeliveryId`] en `(génération du canal, delivery_tag)`.
fn decode_delivery(id: &DeliveryId) -> (u16, u64) {
    ((id.raw() >> TAG_BITS) as u16, id.raw() & TAG_MASK)
}

/// Forme filaire d'un [`Message`] (en-têtes + corps) sérialisée en JSON.
#[derive(Serialize, Deserialize)]
struct WireMessage {
    headers: BTreeMap<String, String>,
    body: serde_json::Value,
}

/// Consommateur `basic_consume` partagé par les tâches d'un worker : une
/// seule à la fois attend le message suivant.
type SharedConsumer = Arc<Mutex<Consumer>>;

/// Canal courant et ce qui ne vaut que pour lui ; remplacé en bloc à chaque
/// reconnexion, ce qui vide les caches.
struct Session {
    channel: Channel,
    /// Incrémentée à chaque reconnexion (voir [`DeliveryId`]).
    generation: u16,
    /// Queues déjà déclarées (avec leur DLQ) sur cette connexion.
    declared: StdMutex<HashSet<String>>,
    /// Consommateur de chaque queue lue par `receive`.
    consumers: Mutex<HashMap<String, SharedConsumer>>,
}

impl Session {
    fn new(channel: Channel, generation: u16) -> Self {
        Self {
            channel,
            generation,
            declared: StdMutex::new(HashSet::new()),
            consumers: Mutex::new(HashMap::new()),
        }
    }

    fn declared(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.declared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Oublie la consommation de `name` si `consumer` est toujours le sien
    /// (flux terminé : le suivant `receive` en ouvre un neuf).
    async fn drop_consumer(&self, name: &str, consumer: &SharedConsumer) {
        let mut consumers = self.consumers.lock().await;
        if consumers
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, consumer))
        {
            consumers.remove(name);
        }
    }
}

/// Connexion courante et sa session.
struct Link {
    conn: Connection,
    session: Arc<Session>,
}

/// File de messages adossée à RabbitMQ, reconnectée à la demande.
pub struct RabbitMqQueue {
    url: String,
    prefetch: u16,
    link: RwLock<Link>,
    /// Queues dont ce process a cessé la consommation (`stop_receiving`).
    stopped: StdMutex<HashSet<String>>,
}

impl RabbitMqQueue {
    /// Se connecte au broker (`amqp://user:pass@host:5672/vhost`) et ouvre un
    /// canal. Branche l'exécuteur/réacteur **tokio** pour tourner sur le
    /// runtime de l'application.
    ///
    /// Le vhost est le **chemin** de l'URL : le vhost par défaut `/` s'écrit
    /// `amqp://…:5672/%2f` ; une barre finale seule (`…:5672/`) désigne le
    /// vhost vide, refusé par le broker (`NOT_ALLOWED - vhost not found`).
    pub async fn connect(amqp_url: &str) -> autome_core::Result<Self> {
        Self::try_connect(amqp_url)
            .await
            .map_err(|e| storage_err("connexion", e))
    }

    /// Comme [`connect`](Self::connect), en retentant pendant `timeout` les
    /// **erreurs réseau** (broker pas encore à l'écoute). Une erreur de
    /// protocole (identifiants, vhost…) est définitive et remonte aussitôt.
    pub async fn connect_retrying(amqp_url: &str, timeout: Duration) -> autome_core::Result<Self> {
        crate::retry::retry_until(
            "rabbitmq",
            timeout,
            Duration::from_millis(500),
            Duration::from_secs(5),
            |e: &lapin::Error| matches!(e, lapin::Error::IOError(_)),
            || Self::try_connect(amqp_url),
        )
        .await
        .map_err(|e| storage_err("connexion", e))
    }

    async fn try_connect(amqp_url: &str) -> Result<Self, lapin::Error> {
        let (conn, channel) = Self::open(amqp_url).await?;
        Ok(Self {
            url: amqp_url.to_string(),
            prefetch: DEFAULT_PREFETCH,
            link: RwLock::new(Link {
                conn,
                session: Arc::new(Session::new(channel, 0)),
            }),
            stopped: StdMutex::new(HashSet::new()),
        })
    }

    /// Builder : nombre de messages non acquittés que le broker pousse
    /// d'avance au consommateur de [`Queue::receive`] (plancher 1). Le
    /// worker y met sa concurrence : chaque tâche a son prochain message
    /// sous la main sans en retenir davantage, les autres répliques gardent
    /// le reste de la file.
    pub fn with_prefetch(mut self, prefetch: u16) -> Self {
        self.prefetch = prefetch.max(1);
        self
    }

    /// Ouvre connexion + canal en mode publisher confirms.
    async fn open(amqp_url: &str) -> Result<(Connection, Channel), lapin::Error> {
        let props = ConnectionProperties::default()
            .with_executor(tokio_executor_trait::Tokio::current())
            .with_reactor(tokio_reactor_trait::Tokio);
        let conn = Connection::connect(amqp_url, props).await?;
        let channel = conn.create_channel().await?;
        channel
            .confirm_select(ConfirmSelectOptions::default())
            .await?;
        Ok((conn, channel))
    }

    /// Session utilisable ; rouvre la connexion si le canal est fermé (une
    /// tentative, bornée à [`OPEN_TIMEOUT`]).
    async fn session(&self) -> autome_core::Result<Arc<Session>> {
        {
            let link = self.link.read().await;
            if link.session.channel.status().connected() {
                return Ok(Arc::clone(&link.session));
            }
        }
        let mut link = self.link.write().await;
        // Un autre appelant a pu reconnecter pendant l'attente du verrou.
        if link.session.channel.status().connected() {
            return Ok(Arc::clone(&link.session));
        }
        let (conn, channel) = tokio::time::timeout(OPEN_TIMEOUT, Self::open(&self.url))
            .await
            .map_err(|_| storage_err("reconnexion", "délai dépassé"))?
            .map_err(|e| storage_err("reconnexion", e))?;
        let generation = link.session.generation.wrapping_add(1);
        let session = Arc::new(Session::new(channel, generation));
        *link = Link {
            conn,
            session: Arc::clone(&session),
        };
        tracing::warn!(generation, "connexion RabbitMQ rétablie");
        Ok(session)
    }

    /// Ferme la connexion courante (tests de résilience : simule la perte du
    /// broker). L'appel suivant reconnecte.
    #[doc(hidden)]
    pub async fn close_connection_for_tests(&self) {
        let link = self.link.read().await;
        let _ = link.conn.close(320, "test de reconnexion").await;
    }

    /// Déclare `name` (durable, avec DLX → `dlq_of(name)`) **et** sa DLQ,
    /// une fois par connexion. Idempotent (mêmes arguments).
    async fn ensure_queues(session: &Session, name: &str) -> autome_core::Result<()> {
        if session.declared().contains(name) {
            return Ok(());
        }
        let channel = &session.channel;
        let dlq = dlq_of(name);
        // DLQ d'abord (cible du dead-lettering).
        channel
            .queue_declare(
                &dlq,
                QueueDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| storage_err("declare dlq", e))?;
        // Queue principale avec dead-letter-exchange par défaut + routing key DLQ.
        let mut args = FieldTable::default();
        args.insert(
            "x-dead-letter-exchange".into(),
            AMQPValue::LongString("".into()),
        );
        args.insert(
            "x-dead-letter-routing-key".into(),
            AMQPValue::LongString(dlq.as_str().into()),
        );
        channel
            .queue_declare(
                name,
                QueueDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                args,
            )
            .await
            .map_err(|e| storage_err("declare", e))?;
        session.declared().insert(name.to_string());
        Ok(())
    }

    /// Consommateur de `name` sur cette session, créé au premier appel
    /// (`basic_qos(prefetch)` puis `basic_consume`, acquittement manuel).
    async fn consumer(&self, session: &Session, name: &str) -> autome_core::Result<SharedConsumer> {
        let mut consumers = session.consumers.lock().await;
        if let Some(consumer) = consumers.get(name) {
            return Ok(Arc::clone(consumer));
        }
        Self::ensure_queues(session, name).await?;
        session
            .channel
            .basic_qos(self.prefetch, BasicQosOptions::default())
            .await
            .map_err(|e| storage_err("qos", e))?;
        let consumer = session
            .channel
            .basic_consume(
                name,
                "",
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|e| storage_err("consume", e))?;
        tracing::info!(
            queue = name,
            prefetch = self.prefetch,
            generation = session.generation,
            "consommateur RabbitMQ démarré"
        );
        let consumer = Arc::new(Mutex::new(consumer));
        consumers.insert(name.to_string(), Arc::clone(&consumer));
        Ok(consumer)
    }

    /// Convertit une livraison AMQP. Un corps illisible part en DLQ : non
    /// acquitté, il resterait en vol jusqu'à la fermeture du canal et
    /// occuperait une place de prefetch.
    async fn decode(
        session: &Session,
        name: &str,
        delivery: lapin::message::Delivery,
    ) -> autome_core::Result<Delivery> {
        match serde_json::from_slice::<WireMessage>(&delivery.data) {
            Ok(wire) => Ok(Delivery {
                id: encode_delivery(session.generation, delivery.delivery_tag),
                queue: name.to_string(),
                message: Message {
                    headers: wire.headers,
                    body: wire.body,
                },
                redeliveries: u32::from(delivery.redelivered),
            }),
            Err(e) => {
                let options = BasicNackOptions {
                    requeue: false,
                    ..Default::default()
                };
                if let Err(nack) = session
                    .channel
                    .basic_nack(delivery.delivery_tag, options)
                    .await
                {
                    tracing::warn!(queue = name, error = %nack, "message illisible non écarté");
                }
                Err(storage_err("message illisible, envoyé en DLQ", e))
            }
        }
    }

    fn is_stopped(&self, name: &str) -> bool {
        self.stopped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }

    /// `ack`/`nack` d'une livraison : ignoré si elle vient d'un canal
    /// disparu (le broker l'a remise en file à la fermeture du canal).
    async fn settle(&self, id: &DeliveryId, settle: Settle) -> autome_core::Result<()> {
        let (generation, tag) = decode_delivery(id);
        let channel = {
            let link = self.link.read().await;
            let session = &link.session;
            if generation != session.generation || !session.channel.status().connected() {
                tracing::warn!(
                    verb = settle.verb(),
                    delivery_tag = tag,
                    delivery_generation = generation,
                    current_generation = session.generation,
                    "livraison d'un canal fermé : ignorée, le broker l'a remise en file"
                );
                return Ok(());
            }
            session.channel.clone()
        };
        match settle {
            Settle::Ack => channel.basic_ack(tag, BasicAckOptions::default()).await,
            Settle::Nack { requeue } => {
                channel
                    .basic_nack(
                        tag,
                        BasicNackOptions {
                            requeue,
                            ..Default::default()
                        },
                    )
                    .await
            }
        }
        .map_err(|e| storage_err(settle.verb(), e))
    }
}

/// Règlement d'une livraison.
#[derive(Clone, Copy)]
enum Settle {
    Ack,
    Nack { requeue: bool },
}

impl Settle {
    fn verb(self) -> &'static str {
        match self {
            Self::Ack => "ack",
            Self::Nack { .. } => "nack",
        }
    }
}

#[async_trait]
impl Queue for RabbitMqQueue {
    async fn publish(&self, name: &str, msg: Message) -> autome_core::Result<()> {
        let session = self.session().await?;
        let wire = WireMessage {
            headers: msg.headers,
            body: msg.body,
        };
        let payload = serde_json::to_vec(&wire)?;
        // Deux essais : le second suit un message renvoyé non routé (queue
        // supprimée depuis sa déclaration en cache), après redéclaration.
        for _ in 0..2 {
            Self::ensure_queues(&session, name).await?;
            let confirmation = session
                .channel
                .basic_publish(
                    "",
                    name,
                    BasicPublishOptions {
                        mandatory: true,
                        ..Default::default()
                    },
                    &payload,
                    BasicProperties::default().with_delivery_mode(2), // persistant
                )
                .await
                .map_err(|e| storage_err("publish", e))?
                .await
                .map_err(|e| storage_err("publish confirm", e))?;
            if confirmation.is_nack() {
                return Err(storage_err(
                    "publish",
                    "message refusé par le broker (nack)",
                ));
            }
            if confirmation.take_message().is_none() {
                return Ok(());
            }
            session.declared().remove(name);
            tracing::warn!(queue = name, "message non routé : queue redéclarée");
        }
        Err(storage_err(
            "publish",
            "message non routé (queue introuvable)",
        ))
    }

    async fn poll(&self, name: &str) -> autome_core::Result<Option<Delivery>> {
        let session = self.session().await?;
        Self::ensure_queues(&session, name).await?;
        let got = session
            .channel
            .basic_get(name, BasicGetOptions::default())
            .await
            .map_err(|e| storage_err("poll", e))?;
        let Some(message) = got else {
            return Ok(None);
        };
        Self::decode(&session, name, message.delivery)
            .await
            .map(Some)
    }

    async fn receive(&self, name: &str, wait: Duration) -> autome_core::Result<Option<Delivery>> {
        if self.is_stopped(name) {
            return Ok(None);
        }
        let session = self.session().await?;
        let consumer = self.consumer(&session, name).await?;
        let next = tokio::time::timeout(wait, async {
            let mut consumer = consumer.lock().await;
            consumer.next().await
        })
        .await;
        let delivery = match next {
            // Rien dans le délai.
            Err(_) => return Ok(None),
            Ok(Some(Ok(delivery))) => delivery,
            // Canal fermé : la session est à refaire, le prochain appel
            // reconnecte et rouvre un consommateur.
            Ok(Some(Err(e))) => {
                session.drop_consumer(name, &consumer).await;
                return Err(storage_err("consume", e));
            }
            // Annulé par `stop_receiving` pendant l'attente du verrou.
            Ok(None) if self.is_stopped(name) => return Ok(None),
            // Annulé par le broker (queue supprimée) : redéclarer au suivant.
            Ok(None) => {
                session.drop_consumer(name, &consumer).await;
                session.declared().remove(name);
                return Err(storage_err("consume", "consommateur annulé par le broker"));
            }
        };
        Self::decode(&session, name, delivery).await.map(Some)
    }

    async fn stop_receiving(&self, name: &str) -> autome_core::Result<()> {
        self.stopped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string());
        let session = Arc::clone(&self.link.read().await.session);
        let Some(consumer) = session.consumers.lock().await.remove(name) else {
            return Ok(());
        };
        if !session.channel.status().connected() {
            // Canal fermé : le broker a déjà remis ces messages en file.
            return Ok(());
        }
        // Attend qu'une tâche en `receive` rende la main (au plus son délai).
        let mut consumer = consumer.lock().await;
        session
            .channel
            .basic_cancel(consumer.tag().as_str(), BasicCancelOptions::default())
            .await
            .map_err(|e| storage_err("cancel", e))?;
        // Reçus d'avance mais jamais rendus : de retour dans la file.
        let mut requeued = 0u32;
        while let Ok(Some(Ok(delivery))) =
            tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, consumer.next()).await
        {
            session
                .channel
                .basic_nack(
                    delivery.delivery_tag,
                    BasicNackOptions {
                        requeue: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| storage_err("nack", e))?;
            requeued += 1;
        }
        tracing::info!(queue = name, requeued, "consommateur RabbitMQ arrêté");
        Ok(())
    }

    async fn ack(&self, id: &DeliveryId) -> autome_core::Result<()> {
        self.settle(id, Settle::Ack).await
    }

    async fn nack(&self, id: &DeliveryId, verdict: Nack) -> autome_core::Result<()> {
        let requeue = matches!(verdict, Nack::Requeue);
        self.settle(id, Settle::Nack { requeue }).await
    }

    async fn depth(&self, name: &str) -> autome_core::Result<usize> {
        let session = self.session().await?;
        // Déclaration passive : renvoie le nombre de messages prêts (hors
        // in-flight non-acquittés).
        let q = session
            .channel
            .queue_declare(
                name,
                QueueDeclareOptions {
                    passive: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| storage_err("depth", e))?;
        Ok(q.message_count() as usize)
    }

    async fn ping(&self) -> autome_core::Result<()> {
        // Aller-retour réel avec le broker (un canal marqué ouvert peut
        // appartenir à une connexion morte non encore détectée), sans effet de
        // bord : déclaration passive d'un échange prédéfini par AMQP.
        let session = self.session().await?;
        session
            .channel
            .exchange_declare(
                "amq.direct",
                ExchangeKind::Direct,
                ExchangeDeclareOptions {
                    passive: true,
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| storage_err("ping", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_ids_round_trip_generation_and_tag() {
        for (generation, tag) in [(0, 1), (1, 42), (u16::MAX, TAG_MASK)] {
            assert_eq!(
                decode_delivery(&encode_delivery(generation, tag)),
                (generation, tag)
            );
        }
    }

    #[test]
    fn same_tag_on_another_channel_gives_another_delivery_id() {
        assert_ne!(encode_delivery(0, 7), encode_delivery(1, 7));
        // Génération 0 : identique au tag brut (rétrocompatible).
        assert_eq!(encode_delivery(0, 7).raw(), 7);
    }
}

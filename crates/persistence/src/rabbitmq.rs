//! [`Queue`] adossée à **RabbitMQ** via `lapin` (AMQP async) — dette #107.
//!
//! Sémantique alignée sur `InMemoryQueue` :
//! - `publish` déclare la queue (durable) et publie sur l'exchange par défaut
//!   (routing key = nom de queue) ;
//! - `poll` fait un `basic_get` (un message à la fois), le message reste
//!   in-flight jusqu'à `ack`/`nack` ; le `DeliveryId` **est** le `delivery_tag`
//!   AMQP ;
//! - `nack(Requeue)` → `basic_nack(requeue=true)` ; `nack(DeadLetter)` →
//!   `basic_nack(requeue=false)` : la queue est déclarée avec un **dead-letter
//!   exchange** pointant la DLQ `dlq_of(name)`, donc RabbitMQ y route le message
//!   automatiquement ;
//! - `depth` lit le `message_count` d'une déclaration passive.

use std::collections::BTreeMap;

use async_trait::async_trait;
use autome_core::{dlq_of, Delivery, DeliveryId, Error, Message, Nack, Queue};
use lapin::options::{
    BasicAckOptions, BasicGetOptions, BasicNackOptions, BasicPublishOptions, QueueDeclareOptions,
};
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};
use serde::{Deserialize, Serialize};

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("rabbitmq: {context}: {e}"))
}

/// Forme filaire d'un [`Message`] (en-têtes + corps) sérialisée en JSON.
#[derive(Serialize, Deserialize)]
struct WireMessage {
    headers: BTreeMap<String, String>,
    body: serde_json::Value,
}

/// File de messages adossée à RabbitMQ.
///
/// `Connection` conservée pour garder le lien ouvert ; le `Channel` (clonable,
/// `Send + Sync`) porte les opérations.
pub struct RabbitMqQueue {
    _conn: Connection,
    channel: Channel,
}

impl RabbitMqQueue {
    /// Se connecte au broker (`amqp://user:pass@host:5672/vhost`) et ouvre un
    /// canal. Branche l'exécuteur/réacteur **tokio** pour tourner sur le
    /// runtime de l'application.
    pub async fn connect(amqp_url: &str) -> autome_core::Result<Self> {
        let props = ConnectionProperties::default()
            .with_executor(tokio_executor_trait::Tokio::current())
            .with_reactor(tokio_reactor_trait::Tokio);
        let conn = Connection::connect(amqp_url, props)
            .await
            .map_err(|e| storage_err("connexion", e))?;
        let channel = conn
            .create_channel()
            .await
            .map_err(|e| storage_err("canal", e))?;
        Ok(Self {
            _conn: conn,
            channel,
        })
    }

    /// Déclare `name` (durable, avec DLX → `dlq_of(name)`) **et** sa DLQ.
    /// Idempotent (mêmes arguments).
    async fn ensure_queues(&self, name: &str) -> autome_core::Result<lapin::Queue> {
        let dlq = dlq_of(name);
        // DLQ d'abord (cible du dead-lettering).
        self.channel
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
        let q = self
            .channel
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
        Ok(q)
    }
}

#[async_trait]
impl Queue for RabbitMqQueue {
    async fn publish(&self, name: &str, msg: Message) -> autome_core::Result<()> {
        self.ensure_queues(name).await?;
        let wire = WireMessage {
            headers: msg.headers,
            body: msg.body,
        };
        let payload = serde_json::to_vec(&wire)?;
        self.channel
            .basic_publish(
                "",
                name,
                BasicPublishOptions::default(),
                &payload,
                BasicProperties::default().with_delivery_mode(2), // persistant
            )
            .await
            .map_err(|e| storage_err("publish", e))?
            .await
            .map_err(|e| storage_err("publish confirm", e))?;
        Ok(())
    }

    async fn poll(&self, name: &str) -> autome_core::Result<Option<Delivery>> {
        self.ensure_queues(name).await?;
        let got = self
            .channel
            .basic_get(name, BasicGetOptions::default())
            .await
            .map_err(|e| storage_err("poll", e))?;
        let Some(message) = got else {
            return Ok(None);
        };
        let delivery = message.delivery;
        let wire: WireMessage = serde_json::from_slice(&delivery.data)?;
        Ok(Some(Delivery {
            id: DeliveryId::new(delivery.delivery_tag),
            queue: name.to_string(),
            message: Message {
                headers: wire.headers,
                body: wire.body,
            },
            redeliveries: u32::from(delivery.redelivered),
        }))
    }

    async fn ack(&self, id: &DeliveryId) -> autome_core::Result<()> {
        self.channel
            .basic_ack(id.raw(), BasicAckOptions::default())
            .await
            .map_err(|e| storage_err("ack", e))
    }

    async fn nack(&self, id: &DeliveryId, verdict: Nack) -> autome_core::Result<()> {
        let requeue = matches!(verdict, Nack::Requeue);
        self.channel
            .basic_nack(
                id.raw(),
                BasicNackOptions {
                    requeue,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| storage_err("nack", e))
    }

    async fn depth(&self, name: &str) -> autome_core::Result<usize> {
        // Déclaration passive : renvoie le nombre de messages prêts (hors
        // in-flight non-acquittés).
        let q = self
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
}

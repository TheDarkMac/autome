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

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use autome_core::{dlq_of, Delivery, DeliveryId, Error, Message, Nack, Queue};
use lapin::options::{
    BasicAckOptions, BasicGetOptions, BasicNackOptions, BasicPublishOptions, ConfirmSelectOptions,
    ExchangeDeclareOptions, QueueDeclareOptions,
};
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties, ExchangeKind};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Délai maximal d'ouverture d'une connexion (évite de bloquer les appelants
/// indéfiniment sur un hôte qui ne répond pas).
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Connexion + canal courants, et leur génération (incrémentée à chaque
/// reconnexion).
struct Link {
    conn: Connection,
    channel: Channel,
    generation: u16,
}

/// File de messages adossée à RabbitMQ, reconnectée à la demande.
pub struct RabbitMqQueue {
    url: String,
    link: RwLock<Link>,
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
            link: RwLock::new(Link {
                conn,
                channel,
                generation: 0,
            }),
        })
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

    /// Canal utilisable et sa génération ; rouvre la connexion si le canal
    /// est fermé (une tentative, bornée à [`OPEN_TIMEOUT`]).
    async fn channel(&self) -> autome_core::Result<(Channel, u16)> {
        {
            let link = self.link.read().await;
            if link.channel.status().connected() {
                return Ok((link.channel.clone(), link.generation));
            }
        }
        let mut link = self.link.write().await;
        // Un autre appelant a pu reconnecter pendant l'attente du verrou.
        if link.channel.status().connected() {
            return Ok((link.channel.clone(), link.generation));
        }
        let (conn, channel) = tokio::time::timeout(OPEN_TIMEOUT, Self::open(&self.url))
            .await
            .map_err(|_| storage_err("reconnexion", "délai dépassé"))?
            .map_err(|e| storage_err("reconnexion", e))?;
        let generation = link.generation.wrapping_add(1);
        *link = Link {
            conn,
            channel: channel.clone(),
            generation,
        };
        tracing::warn!(generation, "connexion RabbitMQ rétablie");
        Ok((channel, generation))
    }

    /// Ferme la connexion courante (tests de résilience : simule la perte du
    /// broker). L'appel suivant reconnecte.
    #[doc(hidden)]
    pub async fn close_connection_for_tests(&self) {
        let link = self.link.read().await;
        let _ = link.conn.close(320, "test de reconnexion").await;
    }

    /// Déclare `name` (durable, avec DLX → `dlq_of(name)`) **et** sa DLQ.
    /// Idempotent (mêmes arguments).
    async fn ensure_queues(channel: &Channel, name: &str) -> autome_core::Result<lapin::Queue> {
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
        let q = channel
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

    /// `ack`/`nack` d'une livraison : ignoré si elle vient d'un canal
    /// disparu (le broker l'a remise en file à la fermeture du canal).
    async fn settle(&self, id: &DeliveryId, settle: Settle) -> autome_core::Result<()> {
        let (generation, tag) = decode_delivery(id);
        let channel = {
            let link = self.link.read().await;
            if generation != link.generation || !link.channel.status().connected() {
                tracing::warn!(
                    verb = settle.verb(),
                    delivery_tag = tag,
                    delivery_generation = generation,
                    current_generation = link.generation,
                    "livraison d'un canal fermé : ignorée, le broker l'a remise en file"
                );
                return Ok(());
            }
            link.channel.clone()
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
        let (channel, _) = self.channel().await?;
        Self::ensure_queues(&channel, name).await?;
        let wire = WireMessage {
            headers: msg.headers,
            body: msg.body,
        };
        let payload = serde_json::to_vec(&wire)?;
        let confirmation = channel
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
        if confirmation.is_nack() {
            return Err(storage_err(
                "publish",
                "message refusé par le broker (nack)",
            ));
        }
        Ok(())
    }

    async fn poll(&self, name: &str) -> autome_core::Result<Option<Delivery>> {
        let (channel, generation) = self.channel().await?;
        Self::ensure_queues(&channel, name).await?;
        let got = channel
            .basic_get(name, BasicGetOptions::default())
            .await
            .map_err(|e| storage_err("poll", e))?;
        let Some(message) = got else {
            return Ok(None);
        };
        let delivery = message.delivery;
        let wire: WireMessage = serde_json::from_slice(&delivery.data)?;
        Ok(Some(Delivery {
            id: encode_delivery(generation, delivery.delivery_tag),
            queue: name.to_string(),
            message: Message {
                headers: wire.headers,
                body: wire.body,
            },
            redeliveries: u32::from(delivery.redelivered),
        }))
    }

    async fn ack(&self, id: &DeliveryId) -> autome_core::Result<()> {
        self.settle(id, Settle::Ack).await
    }

    async fn nack(&self, id: &DeliveryId, verdict: Nack) -> autome_core::Result<()> {
        let requeue = matches!(verdict, Nack::Requeue);
        self.settle(id, Settle::Nack { requeue }).await
    }

    async fn depth(&self, name: &str) -> autome_core::Result<usize> {
        let (channel, _) = self.channel().await?;
        // Déclaration passive : renvoie le nombre de messages prêts (hors
        // in-flight non-acquittés).
        let q = channel
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
        let (channel, _) = self.channel().await?;
        channel
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

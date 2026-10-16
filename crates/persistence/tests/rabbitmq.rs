//! Test d'intégration de [`RabbitMqQueue`] contre un vrai RabbitMQ.
//!
//! **Gaté** par `TEST_AMQP_URL` (skip-pass sinon ; la CI le fournit, job `backend`).
//! En local :
//!
//! ```bash
//! docker run --rm -d -p 5672:5672 rabbitmq:3-alpine
//! TEST_AMQP_URL=amqp://guest:guest@localhost:5672/%2f \
//!   cargo test -p autome-persistence --test rabbitmq
//! ```

use autome_core::{dlq_of, Message, Nack, Queue};
use autome_persistence::RabbitMqQueue;
use serde_json::json;

/// Nom de queue unique par run (évite la contamination entre exécutions).
fn unique_queue() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test.runs.{nanos}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_poll_ack_and_dead_letter() {
    let Some(url) = std::env::var("TEST_AMQP_URL").ok() else {
        eprintln!("TEST_AMQP_URL absent → test RabbitMQ sauté");
        return;
    };
    let q = RabbitMqQueue::connect(&url).await.expect("connexion");
    let name = unique_queue();

    // Publish → depth 1.
    q.publish(&name, Message::new(json!({ "n": 1 })))
        .await
        .unwrap();
    assert_eq!(q.depth(&name).await.unwrap(), 1);

    // Poll → message livré (in-flight ⇒ depth 0), corps fidèle.
    let d = q.poll(&name).await.unwrap().expect("un message");
    assert_eq!(d.message.body, json!({ "n": 1 }));
    assert_eq!(d.redeliveries, 0);
    assert_eq!(q.depth(&name).await.unwrap(), 0);

    // Ack → reste à 0 (consommé).
    q.ack(&d.id).await.unwrap();
    assert_eq!(q.depth(&name).await.unwrap(), 0);

    // Dead-letter : publish, poll, nack(DeadLetter) → routé vers la DLQ.
    q.publish(&name, Message::new(json!({ "n": 2 })))
        .await
        .unwrap();
    let d2 = q.poll(&name).await.unwrap().expect("un message");
    q.nack(&d2.id, Nack::DeadLetter).await.unwrap();

    // La DLQ reçoit le message (petit délai de routage côté broker).
    let dlq = dlq_of(&name);
    let mut depth = 0;
    for _ in 0..20 {
        depth = q.depth(&dlq).await.unwrap();
        if depth == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(depth, 1, "le message nack(DeadLetter) doit atterrir en DLQ");
    assert_eq!(q.depth(&name).await.unwrap(), 0);

    // Requeue : publish, poll, nack(Requeue) → de retour, redelivered.
    q.publish(&name, Message::new(json!({ "n": 3 })))
        .await
        .unwrap();
    let d3 = q.poll(&name).await.unwrap().expect("un message");
    q.nack(&d3.id, Nack::Requeue).await.unwrap();
    let d3b = q.poll(&name).await.unwrap().expect("remis en file");
    assert_eq!(d3b.message.body, json!({ "n": 3 }));
    assert_eq!(d3b.redeliveries, 1, "AMQP marque le message redelivered");
    q.ack(&d3b.id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_after_losing_the_connection_and_ignores_stale_acks() {
    let Some(url) = std::env::var("TEST_AMQP_URL").ok() else {
        eprintln!("TEST_AMQP_URL absent → test RabbitMQ sauté");
        return;
    };
    let q = RabbitMqQueue::connect(&url).await.expect("connexion");
    let name = unique_queue();

    q.publish(&name, Message::new(json!({ "n": 1 })))
        .await
        .unwrap();
    let in_flight = q.poll(&name).await.unwrap().expect("message 1 livré");

    // Perte du broker / du réseau : le canal se ferme avec une livraison en vol.
    q.close_connection_for_tests().await;

    // L'ack d'une livraison du canal disparu est ignoré (pas d'erreur, pas
    // d'ack d'un autre message sur le canal neuf).
    q.ack(&in_flight.id).await.unwrap();

    // Le prochain appel reconnecte : la publication repart (confirmée).
    q.publish(&name, Message::new(json!({ "n": 2 })))
        .await
        .unwrap();

    // Le message 1 a été remis en file par le broker à la fermeture du canal.
    let mut seen = Vec::new();
    for _ in 0..50 {
        if let Some(d) = q.poll(&name).await.unwrap() {
            seen.push((d.message.body["n"].as_i64().unwrap(), d.redeliveries));
            q.ack(&d.id).await.unwrap();
            if seen.len() == 2 {
                break;
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    seen.sort();
    assert_eq!(seen, vec![(1, 1), (2, 0)], "1 redélivré, 2 neuf");
    assert_eq!(q.depth(&name).await.unwrap(), 0);
}

/// Sonde de disponibilité (#192) : `ping` fait un aller-retour réel et rouvre
/// la connexion perdue, comme les autres opérations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_round_trips_and_reconnects_after_a_lost_connection() {
    let Some(url) = std::env::var("TEST_AMQP_URL").ok() else {
        eprintln!("TEST_AMQP_URL absent → test RabbitMQ sauté");
        return;
    };
    let queue = RabbitMqQueue::connect(&url).await.expect("connexion");
    queue.ping().await.expect("ping nominal");
    queue.close_connection_for_tests().await;
    queue.ping().await.expect("ping après reconnexion");
}

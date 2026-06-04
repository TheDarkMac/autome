//! Test d'intégration de [`RabbitMqQueue`] contre un vrai RabbitMQ.
//!
//! **Gaté** par `TEST_AMQP_URL` (skip-pass sinon — la CI n'a pas de broker).
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

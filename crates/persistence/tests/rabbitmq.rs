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

// ───── #237 — consommation push (`basic_consume` + prefetch) ─────

/// Délai d'attente d'un message poussé dans les tests.
const WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Profondeur de `name` une fois stabilisée à `expected` (le broker met à jour
/// ses compteurs de façon asynchrone).
async fn depth_settles(q: &RabbitMqQueue, name: &str, expected: usize) -> usize {
    let mut depth = usize::MAX;
    for _ in 0..40 {
        depth = q.depth(name).await.unwrap();
        if depth == expected {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    depth
}

/// Connexion `lapin` brute, hors de `RabbitMqQueue` (manipulations du broker
/// qu'aucune méthode de la file ne fait).
async fn raw_channel(url: &str) -> lapin::Channel {
    let props = lapin::ConnectionProperties::default()
        .with_executor(tokio_executor_trait::Tokio::current())
        .with_reactor(tokio_reactor_trait::Tokio);
    let conn = lapin::Connection::connect(url, props).await.unwrap();
    let channel = conn.create_channel().await.unwrap();
    // La connexion vit avec le canal le temps du test.
    std::mem::forget(conn);
    channel
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_gets_pushed_messages_and_times_out_on_an_empty_queue() {
    let Some(url) = std::env::var("TEST_AMQP_URL").ok() else {
        eprintln!("TEST_AMQP_URL absent → test RabbitMQ sauté");
        return;
    };
    let q = RabbitMqQueue::connect(&url).await.expect("connexion");
    let name = unique_queue();

    // File vide : rend la main après le délai, sans message.
    let started = std::time::Instant::now();
    let short = std::time::Duration::from_millis(200);
    assert!(q.receive(&name, short).await.unwrap().is_none());
    assert!(started.elapsed() >= short, "{:?}", started.elapsed());

    // Un message publié pendant l'attente est poussé aussitôt.
    let waiting = q.receive(&name, WAIT);
    let publish = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        q.publish(&name, Message::new(json!({ "n": 1 }))).await
    };
    let (got, published) = tokio::join!(waiting, publish);
    published.unwrap();
    let d = got.unwrap().expect("message poussé");
    assert_eq!(d.message.body, json!({ "n": 1 }));
    assert_eq!(d.redeliveries, 0);

    // Même règlement que `poll` : requeue ⇒ redélivré, ack ⇒ consommé.
    q.nack(&d.id, Nack::Requeue).await.unwrap();
    let again = q.receive(&name, WAIT).await.unwrap().expect("redélivré");
    assert_eq!(again.message.body, json!({ "n": 1 }));
    assert_eq!(again.redeliveries, 1);
    q.ack(&again.id).await.unwrap();
    q.stop_receiving(&name).await.unwrap();
    assert_eq!(depth_settles(&q, &name, 0).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetch_bounds_the_messages_held_and_stop_receiving_returns_them() {
    let Some(url) = std::env::var("TEST_AMQP_URL").ok() else {
        eprintln!("TEST_AMQP_URL absent → test RabbitMQ sauté");
        return;
    };
    let q = RabbitMqQueue::connect(&url)
        .await
        .expect("connexion")
        .with_prefetch(2);
    let name = unique_queue();
    for n in 0..5 {
        q.publish(&name, Message::new(json!({ "n": n })))
            .await
            .unwrap();
    }

    // Un message rendu + un reçu d'avance : 3 restent prêts pour les autres.
    let first = q.receive(&name, WAIT).await.unwrap().expect("message 0");
    assert_eq!(first.message.body, json!({ "n": 0 }));
    assert_eq!(depth_settles(&q, &name, 3).await, 3, "prefetch de 2");

    // Arrêt : le message reçu d'avance retourne en file, celui rendu reste
    // à acquitter.
    q.stop_receiving(&name).await.unwrap();
    assert_eq!(depth_settles(&q, &name, 4).await, 4);
    q.ack(&first.id).await.unwrap();
    assert!(
        q.receive(&name, std::time::Duration::from_millis(100))
            .await
            .unwrap()
            .is_none(),
        "plus rien après stop_receiving"
    );

    // Les autres consommateurs récupèrent les 4 restants, dans l'ordre.
    let other = RabbitMqQueue::connect(&url).await.expect("connexion");
    let mut seen = Vec::new();
    while let Some(d) = other.receive(&name, WAIT).await.unwrap() {
        seen.push(d.message.body["n"].as_i64().unwrap());
        other.ack(&d.id).await.unwrap();
        if seen.len() == 4 {
            break;
        }
    }
    assert_eq!(seen, vec![1, 2, 3, 4]);
    other.stop_receiving(&name).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_resumes_on_a_new_consumer_after_a_lost_connection() {
    let Some(url) = std::env::var("TEST_AMQP_URL").ok() else {
        eprintln!("TEST_AMQP_URL absent → test RabbitMQ sauté");
        return;
    };
    let q = RabbitMqQueue::connect(&url)
        .await
        .expect("connexion")
        .with_prefetch(4);
    let name = unique_queue();
    for n in 1..=2 {
        q.publish(&name, Message::new(json!({ "n": n })))
            .await
            .unwrap();
    }
    let in_flight = q.receive(&name, WAIT).await.unwrap().expect("message 1");

    q.close_connection_for_tests().await;
    // Le flux du consommateur fermé remonte une erreur (le worker temporise)…
    let mut resumed = None;
    for _ in 0..5 {
        if let Ok(Some(d)) = q.receive(&name, WAIT).await {
            resumed = Some(d);
            break;
        }
    }
    // … puis la reconnexion ouvre un consommateur neuf : les deux messages
    // (en vol et reçu d'avance) reviennent, marqués redélivrés.
    let mut seen = Vec::new();
    let mut next = resumed;
    while let Some(d) = next {
        seen.push((d.message.body["n"].as_i64().unwrap(), d.redeliveries));
        q.ack(&d.id).await.unwrap();
        if seen.len() == 2 {
            break;
        }
        next = q.receive(&name, WAIT).await.unwrap();
    }
    seen.sort();
    assert_eq!(seen, vec![(1, 1), (2, 1)]);
    // L'ack tardif de la livraison du canal fermé est ignoré.
    q.ack(&in_flight.id).await.unwrap();
    q.stop_receiving(&name).await.unwrap();
    assert_eq!(depth_settles(&q, &name, 0).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_redeclares_a_queue_deleted_after_being_cached() {
    let Some(url) = std::env::var("TEST_AMQP_URL").ok() else {
        eprintln!("TEST_AMQP_URL absent → test RabbitMQ sauté");
        return;
    };
    let q = RabbitMqQueue::connect(&url).await.expect("connexion");
    let name = unique_queue();
    q.publish(&name, Message::new(json!({ "n": 1 })))
        .await
        .unwrap();

    // Supprimée par ailleurs : la déclaration en cache est périmée.
    let admin = raw_channel(&url).await;
    admin
        .queue_delete(&name, lapin::options::QueueDeleteOptions::default())
        .await
        .unwrap();

    // Renvoyé non routé, la queue est redéclarée et le message republié.
    q.publish(&name, Message::new(json!({ "n": 2 })))
        .await
        .unwrap();
    let d = q.poll(&name).await.unwrap().expect("message republié");
    assert_eq!(d.message.body, json!({ "n": 2 }));
    q.ack(&d.id).await.unwrap();
    // La DLQ est redéclarée avec elle : le dead-lettering marche encore.
    q.publish(&name, Message::new(json!({ "n": 3 })))
        .await
        .unwrap();
    let d = q.poll(&name).await.unwrap().expect("message 3");
    q.nack(&d.id, Nack::DeadLetter).await.unwrap();
    assert_eq!(depth_settles(&q, &dlq_of(&name), 1).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_message_goes_to_the_dlq_instead_of_holding_a_prefetch_slot() {
    let Some(url) = std::env::var("TEST_AMQP_URL").ok() else {
        eprintln!("TEST_AMQP_URL absent → test RabbitMQ sauté");
        return;
    };
    let q = RabbitMqQueue::connect(&url).await.expect("connexion");
    let name = unique_queue();
    // Déclare la queue et sa DLQ, puis glisse un corps non JSON devant un
    // message valide.
    q.publish(&name, Message::new(json!({ "n": 0 })))
        .await
        .unwrap();
    let d = q.poll(&name).await.unwrap().unwrap();
    q.ack(&d.id).await.unwrap();
    let raw = raw_channel(&url).await;
    // Confirmé avant la publication suivante : l'ordre dans la file est sûr.
    raw.confirm_select(lapin::options::ConfirmSelectOptions::default())
        .await
        .unwrap();
    raw.basic_publish(
        "",
        &name,
        lapin::options::BasicPublishOptions::default(),
        b"pas du json",
        lapin::BasicProperties::default(),
    )
    .await
    .unwrap()
    .await
    .unwrap();
    q.publish(&name, Message::new(json!({ "n": 1 })))
        .await
        .unwrap();

    // Prefetch de 1 : sans écarter le message illisible, le suivant ne
    // serait jamais poussé.
    assert!(q.receive(&name, WAIT).await.is_err(), "corps illisible");
    let d = q
        .receive(&name, WAIT)
        .await
        .unwrap()
        .expect("message suivant");
    assert_eq!(d.message.body, json!({ "n": 1 }));
    q.ack(&d.id).await.unwrap();
    assert_eq!(depth_settles(&q, &dlq_of(&name), 1).await, 1);
    q.stop_receiving(&name).await.unwrap();
}

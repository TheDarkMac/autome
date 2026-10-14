# ADR 0003 — Choix du broker de messages

- **Statut** : Accepté
- **Date** : 2026-05-21
- **Feature** : F5.1.a (cf. [ROADMAP](../../claude-code/ROADMAP.md)) — issue #49
- **Décideurs** : équipe Autome
- **Contexte ADR** : précise l'[ADR initial d'architecture](../ARCHITECTURE.md#7-choix-techniques-structurants-adr-initial) (« file de messages externalisée »)

---

## 1. Contexte

L'orchestrateur (domaine 5 du ROADMAP) doit pouvoir dispatcher des *runs*
de workflow vers un pool de workers distribués, avec :

- **durabilité** : un run accepté ne doit pas être perdu si un worker
  crash ou si le broker redémarre ;
- **ack par message** : un run est complété quand le worker l'a réussi —
  pas avant ;
- **DLQ** : un run qui échoue après ses retries part en dead-letter queue
  pour inspection humaine, pas en boucle infinie ;
- **priorités** : free tier vs paid, runs critiques vs batch ;
- **throughput cible** : milliers de runs/s (cf. F5.6) — pas millions ;
- **ops maîtrisables** : on déploie en équipe restreinte, pas avec un
  SRE dédié au broker.

La sémantique est **task queue** (un message = un travail pour un
worker), pas **event stream** (un événement consommé par N abonnés
indépendants).

## 2. Options

| Broker | Modèle | Avantages | Inconvénients |
|---|---|---|---|
| **RabbitMQ** | AMQP 0.9.1, task queue native | Ack par message, DLQ + retry natifs, priorités, ops simples, écosystème mature (Sidekiq-style, Celery, n8n) | Throughput plafonné (~50k msg/s par node — largement suffisant) |
| Kafka | Log distribué, partitions | Throughput extrême (millions/s), event sourcing / replay | Ack par offset (par partition, pas par message) — mal aligné avec task-queue ; ops lourdes (KRaft/ZK), JVM, tuning |
| NATS JetStream | Streams + consumer groups | Léger, simple à opérer, Rust client natif | Audience plus étroite, moins de retours d'expérience à grande échelle pour des task queues, primitives ack/redelivery plus jeunes |
| Redis Streams | Append-only log + consumer groups | Très simple, déjà déployé fréquemment | Pas de DLQ native ni de priorités au niveau message ; durabilité dépend de l'AOF/replicas |

## 3. Décision

**Nous adoptons RabbitMQ** comme broker de référence pour l'orchestrateur
Autome.

Raisons :

1. **Alignement sémantique.** RabbitMQ est conçu pour la task queue : un
   producteur, des consommateurs en compétition, ack par message, retry
   configurable, DLQ native — exactement ce qu'on veut. Kafka force à
   plier l'API (replay non désiré, offset par partition) ; NATS et Redis
   demandent plus de code maison.
2. **Throughput suffisant.** Cible F5.6 = milliers de runs/s. Un seul
   node RabbitMQ tient ce débit confortablement. Pas besoin du sur-tooling
   de Kafka.
3. **Ops maîtrisables.** Un node RabbitMQ + un Docker image suffisent en
   dev/staging. La HA (deux nodes en mirror queues / quorum queues) reste
   simple comparée à un cluster Kafka avec KRaft.
4. **Client Rust mature.** `lapin` (async) est stable et largement
   utilisé en production. Pas de surcoût d'intégration.

## 4. Conséquences

- L'abstraction `core::queue::Queue` reste **agnostique du broker** :
  trait `publish` / `poll` / `ack` / `nack` + types `Message` / `Delivery`
  / `DeliveryId` / `Nack { Requeue, DeadLetter }`. Le code applicatif
  (worker, api) ne dépend pas de RabbitMQ.
- L'implémentation `InMemoryQueue` (F5.1.c) sert les tests et le mode
  dev. L'implémentation `lapin` réelle sera branchée avec **F5.2**
  (worker pool) — quand elle est effectivement consommée.
- DLQ : convention de nommage `<queue>.dlq` (helper
  [`dlq_of`](../../crates/core/src/queue.rs)). `Nack::DeadLetter` publie
  vers cette DLQ ; côté RabbitMQ ce sera fait via la policy
  `x-dead-letter-exchange` sur la queue principale.
- Priorités : pas couvertes par F5.1 — exposées plus tard via le champ
  `headers` de `Message` (la priorité AMQP est un header `priority`).
- Si plus tard on a besoin d'event sourcing (replay des runs pour debug
  / re-conciliation), on ajoute Kafka **à côté** plutôt qu'à la place :
  Rabbit pour la task queue, Kafka pour le log applicatif.

## 5. Références

- [docs/ARCHITECTURE.md §7](../ARCHITECTURE.md#7-choix-techniques-structurants-adr-initial)
- RabbitMQ — <https://www.rabbitmq.com>
- `lapin` (client AMQP Rust) — <https://github.com/amqp-rs/lapin>

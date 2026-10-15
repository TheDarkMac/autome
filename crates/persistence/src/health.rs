//! Vérifications de disponibilité des backends (#192) et leur exécution.
//!
//! [`run_checks`] lance toutes les vérifications **en parallèle**, chacune
//! bornée par un délai : une base qui ne répond plus (paquets perdus, pas de
//! refus) ne doit pas faire dépasser à la sonde son propre délai côté
//! orchestrateur. Le détail d'une erreur est journalisé ; seule la liste des
//! vérifications en échec sort de la sonde.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use autome_core::{Error, HealthCheck, Queue};

use crate::pool::PgPool;

/// Délai par défaut d'une vérification.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// PostgreSQL joignable : `SELECT 1` sur une connexion du pool.
pub struct PostgresCheck {
    pool: PgPool,
}

impl PostgresCheck {
    /// Vérification sur `pool` (de préférence partagé avec les stores).
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl HealthCheck for PostgresCheck {
    fn name(&self) -> &'static str {
        "postgres"
    }

    async fn check(&self) -> autome_core::Result<()> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| Error::Invalid(format!("pool : {e}")))?;
        client
            .query_one("SELECT 1", &[])
            .await
            .map_err(|e| Error::Invalid(format!("SELECT 1 : {e}")))?;
        Ok(())
    }
}

/// File de messages joignable ([`Queue::ping`]).
pub struct QueueCheck {
    queue: Arc<dyn Queue + Send + Sync>,
    name: &'static str,
}

impl QueueCheck {
    /// Vérification de `queue`, exposée sous `name` (ex. `rabbitmq`).
    pub fn new(queue: Arc<dyn Queue + Send + Sync>, name: &'static str) -> Self {
        Self { queue, name }
    }
}

#[async_trait]
impl HealthCheck for QueueCheck {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn check(&self) -> autome_core::Result<()> {
        self.queue.ping().await
    }
}

/// Résultat d'une sonde : chaque vérification, `true` si elle a réussi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    /// `(nom, réussite)` dans l'ordre des vérifications.
    pub checks: Vec<(&'static str, bool)>,
}

impl Readiness {
    /// `true` si toutes les vérifications ont réussi (vrai sans vérification).
    pub fn is_ready(&self) -> bool {
        self.checks.iter().all(|(_, ok)| *ok)
    }
}

/// Exécute `checks` en parallèle, chacune bornée par `timeout` ; un délai
/// dépassé ou une panique compte comme un échec.
pub async fn run_checks(checks: &[Arc<dyn HealthCheck>], timeout: Duration) -> Readiness {
    let handles: Vec<_> = checks
        .iter()
        .map(|check| {
            let check = Arc::clone(check);
            tokio::spawn(async move { tokio::time::timeout(timeout, check.check()).await })
        })
        .collect();
    let mut results = Vec::with_capacity(checks.len());
    for (check, handle) in checks.iter().zip(handles) {
        let ok = match handle.await {
            Ok(Ok(Ok(()))) => true,
            Ok(Ok(Err(e))) => {
                tracing::warn!(check = check.name(), error = %e, "dépendance indisponible");
                false
            }
            Ok(Err(_)) => {
                tracing::warn!(
                    check = check.name(),
                    timeout_ms = timeout.as_millis() as u64,
                    "dépendance sans réponse"
                );
                false
            }
            Err(e) => {
                tracing::error!(check = check.name(), error = %e, "vérification interrompue");
                false
            }
        };
        results.push((check.name(), ok));
    }
    Readiness { checks: results }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::InMemoryQueue;

    struct Fixed(&'static str, Option<Duration>, bool);

    #[async_trait]
    impl HealthCheck for Fixed {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn check(&self) -> autome_core::Result<()> {
            if let Some(delay) = self.1 {
                tokio::time::sleep(delay).await;
            }
            if self.2 {
                Ok(())
            } else {
                Err(Error::Invalid("en panne".into()))
            }
        }
    }

    fn check(c: Fixed) -> Arc<dyn HealthCheck> {
        Arc::new(c)
    }

    #[tokio::test]
    async fn all_ok_is_ready_and_order_is_kept() {
        let r = run_checks(
            &[check(Fixed("a", None, true)), check(Fixed("b", None, true))],
            CHECK_TIMEOUT,
        )
        .await;
        assert!(r.is_ready());
        assert_eq!(r.checks, vec![("a", true), ("b", true)]);
    }

    #[tokio::test]
    async fn one_failure_makes_it_unready() {
        let r = run_checks(
            &[
                check(Fixed("a", None, true)),
                check(Fixed("b", None, false)),
            ],
            CHECK_TIMEOUT,
        )
        .await;
        assert!(!r.is_ready());
        assert_eq!(r.checks, vec![("a", true), ("b", false)]);
    }

    #[tokio::test]
    async fn a_hanging_check_times_out_and_checks_run_in_parallel() {
        let started = std::time::Instant::now();
        let r = run_checks(
            &[
                check(Fixed("slow-1", Some(Duration::from_secs(30)), true)),
                check(Fixed("slow-2", Some(Duration::from_secs(30)), true)),
            ],
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(r.checks, vec![("slow-1", false), ("slow-2", false)]);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "en parallèle et bornées"
        );
    }

    #[tokio::test]
    async fn no_check_means_ready() {
        assert!(run_checks(&[], CHECK_TIMEOUT).await.is_ready());
    }

    #[tokio::test]
    async fn in_memory_queue_is_always_reachable() {
        let queue: Arc<dyn Queue + Send + Sync> = Arc::new(InMemoryQueue::new());
        let r = run_checks(&[Arc::new(QueueCheck::new(queue, "queue"))], CHECK_TIMEOUT).await;
        assert_eq!(r.checks, vec![("queue", true)]);
    }
}

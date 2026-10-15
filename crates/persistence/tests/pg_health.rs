//! Vérification de disponibilité PostgreSQL (#192) contre un **vrai** serveur.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_revocation.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use autome_core::HealthCheck;
use autome_persistence::{connect, run_checks, PostgresCheck};

#[tokio::test]
async fn reachable_database_is_ready_and_a_dead_one_is_reported_quickly() {
    let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let live: Arc<dyn HealthCheck> = Arc::new(PostgresCheck::new(connect(&url).expect("pool")));
    let r = run_checks(&[live], Duration::from_secs(2)).await;
    assert_eq!(r.checks, vec![("postgres", true)]);

    // Port fermé : refus immédiat, sonde non prête, sans attendre le délai.
    let dead: Arc<dyn HealthCheck> = Arc::new(PostgresCheck::new(
        connect("postgres://postgres:pg@127.0.0.1:1/postgres").expect("pool"),
    ));
    let started = Instant::now();
    let r = run_checks(&[dead], Duration::from_secs(2)).await;
    assert_eq!(r.checks, vec![("postgres", false)]);
    assert!(!r.is_ready());
    assert!(started.elapsed() <= Duration::from_secs(3));
}

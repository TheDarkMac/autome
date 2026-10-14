//! Test d'intégration de [`PgLoginThrottle`] contre un **vrai** PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_revocation.rs`.
//!
//! Schéma créé depuis la migration réelle `0012`. Les tests partagent la table
//! et tournent en parallèle : chacun utilise ses propres emails.

use std::sync::Arc;

use autome_core::{LoginAttempt, LoginThrottleStore, LOGIN_WINDOW_SECONDS, MAX_LOGIN_FAILURES};
use autome_persistence::{connect, PgLoginThrottle, MIGRATIONS};
use tokio::sync::OnceCell;

const START: i64 = 1_700_000_000;

static SCHEMA: OnceCell<()> = OnceCell::const_new();

/// Deux stores sur deux pools distincts : deux répliques de l'API.
async fn setup() -> Option<(PgLoginThrottle, PgLoginThrottle)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let replica_a = connect(&url).expect("pool a");
    let replica_b = connect(&url).expect("pool b");
    SCHEMA
        .get_or_init(|| async {
            let ddl = MIGRATIONS
                .iter()
                .find(|(name, _)| *name == "0012_login_attempts.sql")
                .map(|(_, sql)| *sql)
                .expect("migration 0012 embarquée");
            replica_a
                .get()
                .await
                .expect("connexion")
                .batch_execute(&format!("DROP TABLE IF EXISTS login_attempts; {ddl}"))
                .await
                .expect("schéma de test");
        })
        .await;
    Some((
        PgLoginThrottle::new(replica_a),
        PgLoginThrottle::new(replica_b),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_burst_across_replicas_never_exceeds_the_quota() {
    let Some((a, b)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let (a, b) = (Arc::new(a), Arc::new(b));
    let mut tasks = Vec::new();
    for i in 0..20 {
        let store = if i % 2 == 0 {
            Arc::clone(&a)
        } else {
            Arc::clone(&b)
        };
        tasks.push(tokio::spawn(async move {
            store
                .begin_attempt("burst@example.io", START)
                .await
                .unwrap()
        }));
    }
    let mut allowed = 0;
    for t in tasks {
        if t.await.unwrap() == LoginAttempt::Allowed {
            allowed += 1;
        }
    }
    assert_eq!(
        allowed, MAX_LOGIN_FAILURES,
        "quota global, pas par réplique"
    );
    // L'autre réplique voit le même blocage.
    assert_eq!(
        b.begin_attempt("burst@example.io", START + 60)
            .await
            .unwrap(),
        LoginAttempt::Blocked {
            retry_after_secs: LOGIN_WINDOW_SECONDS - 60
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn window_expiry_reset_and_gc() {
    let Some((a, b)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    for _ in 0..=MAX_LOGIN_FAILURES {
        a.begin_attempt("win@example.io", START).await.unwrap();
    }
    assert!(matches!(
        a.begin_attempt("win@example.io", START + 1).await.unwrap(),
        LoginAttempt::Blocked { .. }
    ));
    // Fenêtre écoulée : nouveau départ, les refus ne l'ont pas prolongée.
    assert_eq!(
        b.begin_attempt("win@example.io", START + LOGIN_WINDOW_SECONDS)
            .await
            .unwrap(),
        LoginAttempt::Allowed
    );

    // Connexion réussie sur une réplique ⇒ compteur remis à zéro pour toutes.
    for _ in 0..=MAX_LOGIN_FAILURES {
        a.begin_attempt("reset@example.io", START).await.unwrap();
    }
    b.reset("reset@example.io").await.unwrap();
    assert_eq!(
        a.begin_attempt("reset@example.io", START).await.unwrap(),
        LoginAttempt::Allowed
    );

    // Purge : seule la fenêtre expirée disparaît.
    a.begin_attempt("gc-old@example.io", START - LOGIN_WINDOW_SECONDS)
        .await
        .unwrap();
    a.begin_attempt("gc-new@example.io", START).await.unwrap();
    let removed = a.gc(START).await.unwrap();
    assert!(removed >= 1, "fenêtre expirée purgée");
    assert_eq!(
        a.begin_attempt("gc-new@example.io", START).await.unwrap(),
        LoginAttempt::Allowed,
        "fenêtre courante conservée (2ᵉ tentative)"
    );
}

//! Test d'intégration de [`PgMfaStore`] contre un vrai PostgreSQL (#245).
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_users.rs`.

use std::collections::HashSet;
use std::sync::Arc;

use autome_core::{CreateUser, MfaRecord, MfaStore, UserStore};
use autome_persistence::{connect, PgMfaStore, PgPool, PgUserStore};
use tokio::sync::MutexGuard;

/// Les tests recréent les mêmes tables : exécution en série.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup() -> Option<(PgMfaStore, PgUserStore, PgPool, MutexGuard<'static, ()>)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let guard = SERIAL.lock().await;
    let pool = connect(&url).expect("pool");
    let client = pool.get().await.expect("connexion");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS mfa_recovery_codes; DROP TABLE IF EXISTS user_mfa; \
             DROP TABLE IF EXISTS user_permissions; \
             CREATE TABLE user_permissions (user_id TEXT NOT NULL, permission TEXT NOT NULL, \
               PRIMARY KEY (user_id, permission)); \
             DROP TABLE IF EXISTS users CASCADE; \
             CREATE TABLE users (id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
               email TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, \
               role TEXT NOT NULL DEFAULT 'user', \
               created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
               updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
               disabled_at TIMESTAMPTZ NULL, email_verified_at TIMESTAMPTZ NULL);",
        )
        .await
        .expect("schéma de test");
    client
        .batch_execute(include_str!("../../../migrations/0022_user_mfa.sql"))
        .await
        .expect("migration 0022");
    Some((
        PgMfaStore::new(pool.clone()),
        PgUserStore::new(pool.clone()),
        pool,
        guard,
    ))
}

async fn account(users: &PgUserStore, email: &str) -> String {
    match users
        .create(email, "$argon2id$h", &HashSet::new(), true)
        .await
        .unwrap()
    {
        CreateUser::Created(u) => u.id,
        CreateUser::EmailTaken => panic!("email déjà pris"),
    }
}

fn hashes(codes: &[&str]) -> Vec<String> {
    codes.iter().map(|c| (*c).to_string()).collect()
}

#[tokio::test]
async fn enrollment_activation_and_replay_protection() {
    let Some((mfa, users, _pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let alice = account(&users, "alice@example.io").await;

    // Compte inconnu ou id mal formé : rien n'est écrit.
    assert!(!mfa
        .begin_enrollment("3f2504e0-4f89-41d3-9a0c-0305e82c3301", "s")
        .await
        .unwrap());
    assert!(!mfa.begin_enrollment("pas-un-uuid", "s").await.unwrap());
    assert_eq!(mfa.get("pas-un-uuid").await.unwrap(), None);

    assert!(mfa.begin_enrollment(&alice, "s1").await.unwrap());
    assert!(mfa.begin_enrollment(&alice, "s2").await.unwrap());
    assert_eq!(
        mfa.get(&alice).await.unwrap(),
        Some(MfaRecord {
            secret: "s2".into(),
            enabled: false,
            last_step: None,
            recovery_codes_left: 0,
        })
    );
    assert!(!mfa.consume_step(&alice, 1).await.unwrap(), "pas active");
    assert!(
        !mfa.enable(&alice, "s1", 100, &[]).await.unwrap(),
        "ancien secret"
    );
    assert!(mfa
        .enable(&alice, "s2", 100, &hashes(&["h1", "h2", "h3"]))
        .await
        .unwrap());
    assert!(!mfa.enable(&alice, "s2", 101, &[]).await.unwrap());
    assert!(!mfa.begin_enrollment(&alice, "s3").await.unwrap(), "active");
    let active = mfa.get(&alice).await.unwrap().unwrap();
    assert_eq!(
        (active.secret.as_str(), active.enabled, active.last_step),
        ("s2", true, Some(100))
    );
    assert_eq!(active.recovery_codes_left, 3);

    assert!(!mfa.consume_step(&alice, 100).await.unwrap(), "rejeu");
    assert!(!mfa.consume_step(&alice, 99).await.unwrap());
    assert!(mfa.consume_step(&alice, 101).await.unwrap());

    let bob = account(&users, "bob@example.io").await;
    assert_eq!(
        mfa.enabled_among(&[alice.clone(), bob.clone(), "x".into()])
            .await
            .unwrap(),
        HashSet::from([alice.clone()])
    );
}

#[tokio::test]
async fn concurrent_uses_of_the_same_code_have_a_single_winner() {
    let Some((mfa, users, _pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let alice = account(&users, "alice@example.io").await;
    mfa.begin_enrollment(&alice, "s").await.unwrap();
    mfa.enable(&alice, "s", 10, &hashes(&["h1"])).await.unwrap();
    let mfa = Arc::new(mfa);

    // Le même pas TOTP présenté 20 fois en même temps : un seul passe.
    let steps = (0..20).map(|_| {
        let (mfa, alice) = (mfa.clone(), alice.clone());
        tokio::spawn(async move { mfa.consume_step(&alice, 11).await.unwrap() })
    });
    let mut won = 0;
    for t in steps {
        won += usize::from(t.await.unwrap());
    }
    assert_eq!(won, 1, "un pas TOTP ne sert qu'une fois");

    // Le même code de secours présenté 20 fois en même temps : un seul passe.
    let codes = (0..20).map(|_| {
        let (mfa, alice) = (mfa.clone(), alice.clone());
        tokio::spawn(async move { mfa.consume_recovery_code(&alice, "h1").await.unwrap() })
    });
    let mut won = 0;
    for t in codes {
        won += usize::from(t.await.unwrap());
    }
    assert_eq!(won, 1, "un code de secours ne sert qu'une fois");
    assert_eq!(
        mfa.get(&alice).await.unwrap().unwrap().recovery_codes_left,
        0
    );
}

#[tokio::test]
async fn recovery_codes_replacement_and_cascade_deletion() {
    let Some((mfa, users, pool, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let alice = account(&users, "alice@example.io").await;
    mfa.begin_enrollment(&alice, "s").await.unwrap();
    assert!(!mfa
        .replace_recovery_codes(&alice, &hashes(&["x"]))
        .await
        .unwrap());
    mfa.enable(&alice, "s", 1, &hashes(&["h1", "h2"]))
        .await
        .unwrap();
    assert!(mfa.consume_recovery_code(&alice, "h1").await.unwrap());
    assert!(mfa
        .replace_recovery_codes(&alice, &hashes(&["h3", "h4", "h4"]))
        .await
        .unwrap());
    assert!(
        !mfa.consume_recovery_code(&alice, "h2").await.unwrap(),
        "remplacé"
    );
    assert_eq!(
        mfa.get(&alice).await.unwrap().unwrap().recovery_codes_left,
        2
    );

    // Désactivation : tout disparaît, réenrôlement possible.
    assert!(mfa.disable(&alice).await.unwrap());
    assert!(!mfa.disable(&alice).await.unwrap());
    assert_eq!(mfa.get(&alice).await.unwrap(), None);
    let client = pool.get().await.unwrap();
    let left: i64 = client
        .query_one("SELECT count(*) FROM mfa_recovery_codes", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 0);

    // Effacement du compte (#229) : le second facteur suit en cascade.
    mfa.begin_enrollment(&alice, "s").await.unwrap();
    mfa.enable(&alice, "s", 1, &hashes(&["h"])).await.unwrap();
    client
        .execute("DELETE FROM users WHERE id = $1::text::uuid", &[&alice])
        .await
        .unwrap();
    let rows: i64 = client
        .query_one(
            "SELECT (SELECT count(*) FROM user_mfa) + (SELECT count(*) FROM mfa_recovery_codes)",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
}

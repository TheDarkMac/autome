//! Test d'intégration de [`PgUserStore`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_permission.rs`.

use std::collections::HashSet;

use autome_core::{CreateUser, PermissionStore, UserStore};
use autome_persistence::{connect, PgPermissionStore, PgUserStore};

async fn setup() -> Option<(PgUserStore, PgPermissionStore)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    let client = pool.get().await.expect("connexion");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS user_permissions; \
             CREATE TABLE user_permissions (user_id TEXT NOT NULL, permission TEXT NOT NULL, \
               PRIMARY KEY (user_id, permission)); \
             DROP TABLE IF EXISTS users CASCADE; \
             CREATE TABLE users (id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
               email TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, \
               role TEXT NOT NULL DEFAULT 'user');",
        )
        .await
        .expect("schéma de test");
    Some((PgUserStore::new(pool.clone()), PgPermissionStore::new(pool)))
}

#[tokio::test]
async fn create_find_and_grant_atomically() {
    let Some((users, perms)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    let granted: HashSet<String> = ["workflows.read".to_string(), "workflows.write".to_string()]
        .into_iter()
        .collect();
    let CreateUser::Created(alice) = users
        .create("alice@example.io", "$argon2id$h1", &granted)
        .await
        .unwrap()
    else {
        panic!("compte attendu");
    };
    assert_eq!(alice.id.len(), 36, "id = UUID texte");
    assert_eq!(perms.permissions_for(&alice.id).await.unwrap(), granted);

    let again = users
        .create("alice@example.io", "$argon2id$h2", &granted)
        .await
        .unwrap();
    assert_eq!(again, CreateUser::EmailTaken);

    let found = users.find_by_email("alice@example.io").await.unwrap();
    assert_eq!(found, Some(alice));
    assert_eq!(
        users.find_by_email("nobody@example.io").await.unwrap(),
        None
    );
}

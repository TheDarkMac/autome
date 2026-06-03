//! Test d'intégration de [`PgPermissionStore`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_revocation.rs`.

use autome_core::PermissionStore;
use autome_persistence::{connect, PgPermissionStore};

async fn setup() -> Option<PgPermissionStore> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    let client = pool.get().await.expect("connexion");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS user_permissions; \
             CREATE TABLE user_permissions (user_id TEXT NOT NULL, permission TEXT NOT NULL, \
               PRIMARY KEY (user_id, permission));",
        )
        .await
        .expect("schéma de test");
    Some(PgPermissionStore::new(pool))
}

#[tokio::test]
async fn grant_revoke_and_permissions_for() {
    let Some(store) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    assert!(store.permissions_for("alice").await.unwrap().is_empty());

    store.grant("alice", "workflows.read").await.unwrap();
    store.grant("alice", "workflows.read").await.unwrap();
    store.grant("alice", "workflows.write").await.unwrap();
    let perms = store.permissions_for("alice").await.unwrap();
    assert_eq!(perms.len(), 2);
    assert!(perms.contains("workflows.read"));
    assert!(perms.contains("workflows.write"));

    assert!(store.permissions_for("bob").await.unwrap().is_empty());

    store.revoke("alice", "workflows.write").await.unwrap();
    store.revoke("alice", "inexistante").await.unwrap();
    let perms = store.permissions_for("alice").await.unwrap();
    assert_eq!(perms.len(), 1);
    assert!(perms.contains("workflows.read"));
}

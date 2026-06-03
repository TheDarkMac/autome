//! Test d'intégration de [`PgPermissionStore`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_revocation.rs`.

use autome_core::PermissionStore;
use autome_persistence::{connect, PgPermissionStore};

fn setup() -> Option<PgPermissionStore> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    let mut client = pool.get().expect("connexion");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS user_permissions; \
             CREATE TABLE user_permissions (user_id TEXT NOT NULL, permission TEXT NOT NULL, \
               PRIMARY KEY (user_id, permission));",
        )
        .expect("schéma de test");
    Some(PgPermissionStore::new(pool))
}

#[test]
fn grant_revoke_and_permissions_for() {
    let Some(store) = setup() else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    // Vide au départ.
    assert!(store.permissions_for("alice").unwrap().is_empty());

    // Grant idempotent.
    store.grant("alice", "workflows.read").unwrap();
    store.grant("alice", "workflows.read").unwrap();
    store.grant("alice", "workflows.write").unwrap();
    let perms = store.permissions_for("alice").unwrap();
    assert_eq!(perms.len(), 2);
    assert!(perms.contains("workflows.read"));
    assert!(perms.contains("workflows.write"));

    // Isolation par utilisateur.
    assert!(store.permissions_for("bob").unwrap().is_empty());

    // Revoke (no-op si absent).
    store.revoke("alice", "workflows.write").unwrap();
    store.revoke("alice", "inexistante").unwrap();
    let perms = store.permissions_for("alice").unwrap();
    assert_eq!(perms.len(), 1);
    assert!(perms.contains("workflows.read"));
}

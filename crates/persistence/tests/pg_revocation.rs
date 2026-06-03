//! Test d'intégration de [`PgRevocationStore`] contre un **vrai** PostgreSQL.
//!
//! **Gaté par environnement** : ne s'exécute que si `TEST_DATABASE_URL` est
//! défini (sinon il *skip* en passant — la CI, sans service Postgres, le saute).
//! En local :
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=pg -p 5440:5432 postgres:16-alpine
//! TEST_DATABASE_URL=postgres://postgres:pg@localhost:5440/postgres \
//!   cargo test -p autome-persistence --test pg_revocation
//! ```

use autome_core::RevocationStore;
use autome_persistence::{connect, PgRevocationStore};

async fn setup() -> Option<PgRevocationStore> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    let client = pool.get().await.expect("connexion");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS revoked_tokens; \
             CREATE TABLE revoked_tokens (jti TEXT PRIMARY KEY, exp_ms BIGINT NOT NULL); \
             CREATE INDEX idx_revoked_tokens_exp ON revoked_tokens (exp_ms);",
        )
        .await
        .expect("schéma de test");
    Some(PgRevocationStore::new(pool))
}

#[tokio::test]
async fn revoke_is_revoked_idempotence_and_gc() {
    let Some(store) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    assert!(!store.is_revoked("jti-a").await.unwrap());

    store.revoke("jti-a", 2_000).await.unwrap();
    assert!(store.is_revoked("jti-a").await.unwrap());

    // Idempotence : un `exp_ms` plus petit ne raccourcit pas ; un plus grand étend.
    store.revoke("jti-a", 1_000).await.unwrap();
    store.revoke("jti-a", 5_000).await.unwrap();

    store.revoke("jti-old", 2_500).await.unwrap();
    let removed = store.gc(3_000).await.unwrap();
    assert_eq!(
        removed, 1,
        "seul jti-old (exp 2500 ≤ 3000) doit être collecté"
    );
    assert!(store.is_revoked("jti-a").await.unwrap());
    assert!(!store.is_revoked("jti-old").await.unwrap());

    assert_eq!(store.gc(10_000).await.unwrap(), 1);
    assert!(!store.is_revoked("jti-a").await.unwrap());
}

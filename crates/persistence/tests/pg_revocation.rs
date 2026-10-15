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

use std::sync::Arc;

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
async fn revoke_is_revoked_idempotence_gc_and_revoke_once() {
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

    // `revoke_once` (#184) : réussit une seule fois, ne touche pas une entrée
    // existante.
    assert!(store.revoke_once("jti-once", 4_000).await.unwrap());
    assert!(store.is_revoked("jti-once").await.unwrap());
    assert!(!store.revoke_once("jti-once", 9_000).await.unwrap());
    store.gc(4_000).await.unwrap();
    assert!(
        !store.is_revoked("jti-once").await.unwrap(),
        "exp_ms d'origine conservé"
    );

    // Rafale concurrente répartie sur deux pools (deux répliques de l'API) :
    // un seul gagnant — c'est ce qui garantit l'usage unique d'un refresh.
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let other = Arc::new(PgRevocationStore::new(connect(&url).expect("pool")));
    let store = Arc::new(store);
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let s: Arc<PgRevocationStore> = if i % 2 == 0 {
                store.clone()
            } else {
                other.clone()
            };
            tokio::spawn(async move { s.revoke_once("jti-race", 9_000).await.unwrap() })
        })
        .collect();
    let mut winners = 0;
    for t in tasks {
        winners += usize::from(t.await.unwrap());
    }
    assert_eq!(winners, 1);
}

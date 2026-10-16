//! Test d'intégration de [`PgRevocationStore`] contre un **vrai** PostgreSQL.
//!
//! **Gaté par environnement** : ne s'exécute que si `TEST_DATABASE_URL` est
//! défini (sinon il *skip* en passant ; la CI le fournit, job `backend`).
//! En local :
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=pg -p 5440:5432 postgres:16-alpine
//! TEST_DATABASE_URL=postgres://postgres:pg@localhost:5440/postgres \
//!   cargo test -p autome-persistence --test pg_revocation
//! ```

use std::sync::Arc;

use autome_core::{RevocationStore, TokenState};
use autome_persistence::{connect, PgRevocationStore};

async fn setup() -> Option<PgRevocationStore> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    let client = pool.get().await.expect("connexion");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS revoked_tokens; \
             CREATE TABLE revoked_tokens (jti TEXT PRIMARY KEY, exp_ms BIGINT NOT NULL); \
             CREATE INDEX idx_revoked_tokens_exp ON revoked_tokens (exp_ms); \
             DROP TABLE IF EXISTS session_versions; \
             CREATE TABLE session_versions (user_id TEXT PRIMARY KEY, \
                                            version BIGINT NOT NULL CHECK (version >= 1));",
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

    // Versions de session (#186) : 0 par défaut, incréments par utilisateur,
    // `token_state` combine les deux lectures en une requête.
    assert_eq!(store.session_version("u").await.unwrap(), 0);
    assert_eq!(store.bump_session_version("u").await.unwrap(), 1);
    assert_eq!(store.session_version("u").await.unwrap(), 1);
    assert_eq!(store.session_version("v").await.unwrap(), 0);
    assert_eq!(
        store.token_state("jti-race", "u").await.unwrap(),
        TokenState {
            revoked: true,
            session_version: 1
        }
    );
    assert_eq!(
        other.token_state("jti-absent", "v").await.unwrap(),
        TokenState {
            revoked: false,
            session_version: 0
        }
    );

    // Incréments concurrents depuis deux pools : aucun n'est perdu, chacun
    // obtient une version distincte.
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let s: Arc<PgRevocationStore> = if i % 2 == 0 {
                store.clone()
            } else {
                other.clone()
            };
            tokio::spawn(async move { s.bump_session_version("w").await.unwrap() })
        })
        .collect();
    let mut versions = Vec::new();
    for t in tasks {
        versions.push(t.await.unwrap());
    }
    versions.sort_unstable();
    assert_eq!(versions, (1..=20).collect::<Vec<i64>>());
    assert_eq!(store.session_version("w").await.unwrap(), 20);

    // Le GC des révocations ne touche pas aux versions de session.
    store.gc(i64::MAX).await.unwrap();
    assert_eq!(other.session_version("u").await.unwrap(), 1);
}

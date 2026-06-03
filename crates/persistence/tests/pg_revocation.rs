//! Test d'intégration de [`PgRevocationStore`] contre un **vrai** PostgreSQL.
//!
//! **Gaté par environnement** : ne s'exécute que si `TEST_DATABASE_URL` est
//! défini (sinon il *skip* en passant — la CI, sans service Postgres, le saute
//! silencieusement). En local :
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=pg -p 5440:5432 postgres:16-alpine
//! TEST_DATABASE_URL=postgres://postgres:pg@localhost:5440/postgres \
//!   cargo test -p autome-persistence --test pg_revocation
//! ```

use autome_core::RevocationStore;
use autome_persistence::{connect, PgRevocationStore};

/// Prépare une table `revoked_tokens` vierge et renvoie un store prêt.
fn setup() -> Option<PgRevocationStore> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    let mut client = pool.get().expect("connexion");
    // Repart d'une table propre (la migration 0006 reproduite à l'identique).
    client
        .batch_execute(
            "DROP TABLE IF EXISTS revoked_tokens; \
             CREATE TABLE revoked_tokens (jti TEXT PRIMARY KEY, exp_ms BIGINT NOT NULL); \
             CREATE INDEX idx_revoked_tokens_exp ON revoked_tokens (exp_ms);",
        )
        .expect("schéma de test");
    Some(PgRevocationStore::new(pool))
}

#[test]
fn revoke_is_revoked_idempotence_and_gc() {
    let Some(store) = setup() else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    // Inconnu au départ.
    assert!(!store.is_revoked("jti-a").unwrap());

    // Révocation → vu comme révoqué.
    store.revoke("jti-a", 2_000).unwrap();
    assert!(store.is_revoked("jti-a").unwrap());

    // Idempotence : un `exp_ms` plus petit ne raccourcit pas la révocation.
    store.revoke("jti-a", 1_000).unwrap();
    // Un `exp_ms` plus grand l'étend.
    store.revoke("jti-a", 5_000).unwrap();

    // GC à t=3000 : `jti-a` (exp 5000) survit ; une entrée expirée part.
    store.revoke("jti-old", 2_500).unwrap();
    let removed = store.gc(3_000).unwrap();
    assert_eq!(
        removed, 1,
        "seul jti-old (exp 2500 ≤ 3000) doit être collecté"
    );
    assert!(store.is_revoked("jti-a").unwrap());
    assert!(!store.is_revoked("jti-old").unwrap());

    // GC au-delà de l'exp de jti-a : il part à son tour.
    assert_eq!(store.gc(10_000).unwrap(), 1);
    assert!(!store.is_revoked("jti-a").unwrap());
}

//! Test d'intégration du runner de migrations contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_revocation.rs`.

use autome_persistence::{connect, run_migrations, MIGRATIONS};

#[tokio::test]
async fn applies_all_then_is_idempotent() {
    let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let pool = connect(&url).expect("pool");

    // Repart d'un schéma vierge (le runner suppose une base qu'il gère).
    {
        let client = pool.get().await.expect("connexion");
        client
            .batch_execute(
                "DROP SCHEMA public CASCADE; CREATE SCHEMA public; \
                 GRANT ALL ON SCHEMA public TO public;",
            )
            .await
            .expect("reset schema");
    }

    // 1er passage : applique toutes les migrations ; 2e : idempotent.
    let applied = run_migrations(&pool).await.expect("run 1");
    assert_eq!(
        applied,
        MIGRATIONS.len(),
        "toutes les migrations appliquées"
    );
    assert_eq!(run_migrations(&pool).await.expect("run 2"), 0);

    let client = pool.get().await.expect("connexion");
    let n: i64 = client
        .query_one("SELECT count(*) FROM schema_migrations", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n as usize, MIGRATIONS.len());

    for table in [
        "users",
        "workflows",
        "revoked_tokens",
        "user_permissions",
        "audit_events",
        "session_versions",
    ] {
        let exists: bool = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
            .await
            .unwrap()
            .get(0);
        assert!(exists, "table absente après migration : {table}");
    }
}

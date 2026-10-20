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
    let credentials: bool = client
        .query_one("SELECT to_regclass('credentials') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    assert!(!credentials, "table morte `credentials` encore présente");

    // Reprise par 0020 (#231) : un compte créé avant la migration est tenu
    // pour vérifié à sa date de création — personne n'est mis à la porte.
    client
        .batch_execute(
            "ALTER TABLE users DROP COLUMN email_verified_at; \
             DELETE FROM schema_migrations WHERE name = '0020_email_verification.sql'; \
             INSERT INTO users (email, password_hash, created_at) \
             VALUES ('legacy@example.io', 'h', '2020-01-01T00:00:00Z');",
        )
        .await
        .unwrap();
    assert_eq!(run_migrations(&pool).await.expect("run 3"), 1);
    let backfilled: bool = client
        .query_one(
            "SELECT email_verified_at = created_at FROM users WHERE email = 'legacy@example.io'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(backfilled, "compte antérieur vérifié à sa date de création");

    // 0023 (#251) sur une base antérieure : la table `credentials` de 0004,
    // indexée par 0005 et seedée, disparaît avec son index.
    client
        .batch_execute(include_str!("../../../migrations/0004_credentials.sql"))
        .await
        .unwrap();
    client
        .batch_execute(
            "CREATE INDEX idx_credentials_owner ON credentials (owner_id); \
             INSERT INTO credentials (owner_id, provider, name, secret_enc) \
             SELECT id, 'slack', 'workspace-dev', '\\x00'::bytea FROM users \
             WHERE email = 'legacy@example.io'; \
             DELETE FROM schema_migrations WHERE name = '0023_drop_credentials.sql';",
        )
        .await
        .unwrap();
    assert_eq!(run_migrations(&pool).await.expect("run 4"), 1);
    let leftovers: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_class \
             WHERE relname IN ('credentials', 'idx_credentials_owner')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(leftovers, 0, "table `credentials` ou son index restant");
}

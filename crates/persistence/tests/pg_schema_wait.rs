//! Attente du schéma par le worker (#271) contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Travaille dans un schéma
//! dédié (`search_path`), sans toucher au schéma `public` des autres tests.

use std::time::{Duration, Instant};

use autome_persistence::{
    connect, pending_migrations, run_migrations, wait_for_schema, PgPool, MIGRATIONS,
};

const SCHEMA: &str = "schema_wait_271";

/// Pool dont les connexions travaillent dans `SCHEMA`, recréé vide.
async fn isolated_pool(url: &str) -> PgPool {
    let admin = connect(url).expect("pool");
    admin
        .get()
        .await
        .expect("connexion")
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};"
        ))
        .await
        .expect("schéma dédié");
    let sep = if url.contains('?') { '&' } else { '?' };
    connect(&format!("{url}{sep}options=-csearch_path%3D{SCHEMA}")).expect("pool isolé")
}

#[tokio::test]
async fn the_worker_waits_for_every_embedded_migration() {
    let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let pool = isolated_pool(&url).await;
    let names: Vec<&str> = MIGRATIONS.iter().map(|(n, _)| *n).collect();
    let last = *names.last().unwrap();

    // Base vierge : pas de journal, tout manque ; la lecture ne le crée pas.
    assert_eq!(pending_migrations(&pool).await.unwrap(), names);
    let client = pool.get().await.unwrap();
    let journal: bool = client
        .query_one("SELECT to_regclass('schema_migrations') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    assert!(!journal, "lecture seule : journal non créé");
    let err = wait_for_schema(&pool, Duration::ZERO).await.unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("{} migration(s)", names.len())),
        "{err}"
    );

    // À jour : rien ne manque, l'attente rend la main aussitôt.
    assert_eq!(run_migrations(&pool).await.unwrap(), names.len());
    assert!(pending_migrations(&pool).await.unwrap().is_empty());
    wait_for_schema(&pool, Duration::ZERO).await.unwrap();

    // Une entrée inconnue du binaire (base migrée par une version plus
    // récente) n'est pas une migration manquante.
    client
        .execute(
            "INSERT INTO schema_migrations (name) VALUES ('9999_future.sql')",
            &[],
        )
        .await
        .unwrap();
    assert!(pending_migrations(&pool).await.unwrap().is_empty());

    // Base en retard d'une migration : attente dépassée, migration nommée.
    client
        .execute("DELETE FROM schema_migrations WHERE name = $1", &[&last])
        .await
        .unwrap();
    assert_eq!(pending_migrations(&pool).await.unwrap(), vec![last]);
    let started = Instant::now();
    let err = wait_for_schema(&pool, Duration::from_millis(800))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(last), "{err}");
    assert!(err.contains("AUTOME_MIGRATE"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(3), "attente bornée");

    // La migration arrive pendant l'attente (l'API vient de migrer) : le
    // worker repart sans attendre la fin du délai.
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move {
            let started = Instant::now();
            wait_for_schema(&pool, Duration::from_secs(30))
                .await
                .map(|()| started.elapsed())
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    client
        .execute("INSERT INTO schema_migrations (name) VALUES ($1)", &[&last])
        .await
        .unwrap();
    let waited = waiter.await.unwrap().expect("schéma prêt");
    assert!(waited < Duration::from_secs(5), "repris en {waited:?}");

    // Base injoignable : l'erreur d'accès remonte une fois le délai écoulé.
    let unreachable = connect("postgres://nobody@127.0.0.1:1/none").unwrap();
    let err = wait_for_schema(&unreachable, Duration::from_millis(600))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("pool"), "{err}");

    drop(client);
    connect(&url)
        .unwrap()
        .get()
        .await
        .unwrap()
        .batch_execute(&format!("DROP SCHEMA {SCHEMA} CASCADE"))
        .await
        .unwrap();
}

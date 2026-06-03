//! Test d'intégration de [`PgAuditSink`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_revocation.rs`.

use autome_core::{AuditEvent, AuditOutcome, AuditSink};
use autome_persistence::{connect, PgAuditSink};
use serde_json::json;

async fn setup() -> Option<PgAuditSink> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    let client = pool.get().await.expect("connexion");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS audit_events; \
             CREATE TABLE audit_events ( \
               id BIGSERIAL PRIMARY KEY, timestamp_ms BIGINT NOT NULL, actor TEXT NOT NULL, \
               action TEXT NOT NULL, resource TEXT, outcome TEXT NOT NULL, \
               metadata JSONB NOT NULL DEFAULT 'null'::jsonb);",
        )
        .await
        .expect("schéma de test");
    Some(PgAuditSink::new(pool))
}

#[tokio::test]
async fn record_then_list_roundtrip_in_insertion_order() {
    let Some(sink) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    assert!(sink.list().await.unwrap().is_empty());

    let e1 = AuditEvent::new(
        1_000,
        "alice",
        "auth.permission_check",
        AuditOutcome::Success,
    )
    .with_resource("workflows.read")
    .with_metadata(json!({ "ip": "10.0.0.1" }));
    let e2 = AuditEvent::new(2_000, "bob", "credentials.create", AuditOutcome::Denied);

    sink.record(e1.clone()).await.unwrap();
    sink.record(e2.clone()).await.unwrap();

    let listed = sink.list().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0], e1);
    assert_eq!(listed[1], e2);
    assert_eq!(listed[1].resource, None);
    assert_eq!(listed[1].outcome, AuditOutcome::Denied);
    assert_eq!(listed[0].metadata, json!({ "ip": "10.0.0.1" }));
}

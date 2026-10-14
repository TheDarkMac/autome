//! Test d'intégration de [`PgConnectorTokenStore`] contre un **vrai**
//! PostgreSQL. Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf.
//! `pg_revocation.rs` pour lancer un conteneur jetable.
//!
//! Le schéma est créé à partir de la migration **réelle** `0011` (embarquée
//! dans [`MIGRATIONS`]) : le test valide aussi le DDL.
//!
//! Runtime **multi-thread** : le trait est synchrone et ponte vers l'async via
//! `block_in_place` (cf. module `connector_tokens`).

use autome_connectors::oauth::{ConnectorTokenStore, TokenSet};
use autome_core::{EnvelopeCipher, InMemoryKeyProvider};
use autome_persistence::{connect, PgConnectorTokenStore, PgPool, MIGRATIONS};
use tokio::sync::OnceCell;

fn cipher(byte: u8) -> EnvelopeCipher<InMemoryKeyProvider> {
    EnvelopeCipher::new(InMemoryKeyProvider::with_key("v1", [byte; 32]))
}

/// Schéma recréé **une fois** par binaire : les tests tournent en parallèle
/// et chacun utilise ses propres `user_id` (pas d'interférence).
static SCHEMA: OnceCell<()> = OnceCell::const_new();

async fn setup() -> Option<PgPool> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = connect(&url).expect("pool");
    SCHEMA
        .get_or_init(|| async {
            let ddl = MIGRATIONS
                .iter()
                .find(|(name, _)| *name == "0011_connector_tokens.sql")
                .map(|(_, sql)| *sql)
                .expect("migration 0011 embarquée");
            let client = pool.get().await.expect("connexion");
            client
                .batch_execute(&format!("DROP TABLE IF EXISTS connector_tokens; {ddl}"))
                .await
                .expect("schéma de test");
        })
        .await;
    Some(pool)
}

fn tokens(access: &str) -> TokenSet {
    TokenSet {
        access_token: access.into(),
        refresh_token: Some(format!("{access}-refresh")),
        expires_at_ms: Some(1_700_000_000_000),
        token_type: "Bearer".into(),
        scope: Some("chat:write".into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn store_load_overwrite_list_delete_isolated_by_user() {
    let Some(pool) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let store = PgConnectorTokenStore::new(pool, cipher(7));

    assert_eq!(store.load("alice", "slack").unwrap(), None);
    store
        .store("alice", "slack", &tokens("xoxb-alice"))
        .unwrap();
    store
        .store("alice", "github", &tokens("ghp-alice"))
        .unwrap();
    store.store("bob", "slack", &tokens("xoxb-bob")).unwrap();

    assert_eq!(
        store.load("alice", "slack").unwrap(),
        Some(tokens("xoxb-alice"))
    );
    assert_eq!(
        store.load("bob", "slack").unwrap(),
        Some(tokens("xoxb-bob"))
    );
    assert_eq!(store.load("bob", "github").unwrap(), None);

    // Upsert : le dernier callback OAuth remplace le précédent.
    store
        .store("alice", "slack", &tokens("xoxb-alice-2"))
        .unwrap();
    assert_eq!(
        store.load("alice", "slack").unwrap().unwrap().access_token,
        "xoxb-alice-2"
    );

    assert_eq!(
        store.list_by_user("alice").unwrap(),
        vec!["github", "slack"]
    );
    assert_eq!(store.list_by_user("carol").unwrap(), Vec::<String>::new());

    store.delete("alice", "slack").unwrap();
    store.delete("alice", "slack").unwrap(); // idempotent
    assert_eq!(store.load("alice", "slack").unwrap(), None);
    assert_eq!(store.list_by_user("alice").unwrap(), vec!["github"]);
    assert!(store.load("bob", "slack").unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_are_encrypted_at_rest_and_survive_a_new_store_instance() {
    let Some(pool) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    PgConnectorTokenStore::new(pool.clone(), cipher(9))
        .store("dana", "slack", &tokens("xoxb-very-secret"))
        .unwrap();

    // Au repos : seule l'enveloppe chiffrée est en base.
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT envelope FROM connector_tokens WHERE user_id = 'dana'",
            &[],
        )
        .await
        .unwrap();
    let envelope: String = row.get(0);
    assert!(
        envelope.starts_with("v1:v1:"),
        "format EnvelopeCipher : {envelope}"
    );
    assert!(!envelope.contains("xoxb-very-secret"));

    // Autre instance (autre process : redémarrage, worker) avec la même clé.
    let reopened = PgConnectorTokenStore::new(pool.clone(), cipher(9));
    assert_eq!(
        reopened
            .load("dana", "slack")
            .unwrap()
            .unwrap()
            .access_token,
        "xoxb-very-secret"
    );
    // Mauvaise clé : échec explicite, jamais un token corrompu.
    assert!(PgConnectorTokenStore::new(pool, cipher(1))
        .load("dana", "slack")
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn works_from_a_spawn_blocking_thread() {
    // Chemin réel du worker (moteur dans `spawn_blocking`) et de l'API.
    let Some(pool) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let store = std::sync::Arc::new(PgConnectorTokenStore::new(pool, cipher(5)));
    let s = std::sync::Arc::clone(&store);
    let loaded = tokio::task::spawn_blocking(move || {
        s.store("erin", "notion", &tokens("secret_alice")).unwrap();
        s.load("erin", "notion").unwrap()
    })
    .await
    .unwrap();
    assert_eq!(loaded.unwrap().access_token, "secret_alice");
}

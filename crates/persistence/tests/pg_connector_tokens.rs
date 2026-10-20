//! Test d'intégration de [`PgConnectorTokenStore`] contre un **vrai**
//! PostgreSQL. Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf.
//! `pg_revocation.rs` pour lancer un conteneur jetable.
//!
//! Le schéma est créé à partir des migrations **réelles** `0011`, `0018` et
//! `0024` (embarquées dans [`MIGRATIONS`]) : le test valide aussi le DDL.
//!
//! Runtime **multi-thread** : le trait est synchrone et ponte vers l'async via
//! `block_in_place` (cf. module `connector_tokens`).

use autome_connectors::oauth::{
    ConnectorTokenStore, CredentialInvalidation, CredentialUpdate, ReconnectNotice,
    ReconnectNotices, StoredCredential, TokenSet,
};
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
            let ddl: String = MIGRATIONS
                .iter()
                .filter(|(name, _)| {
                    [
                        "0011_connector_tokens.sql",
                        "0018_connector_token_invalidation.sql",
                        "0024_connector_token_reconnect_notice.sql",
                    ]
                    .contains(name)
                })
                .map(|(_, sql)| *sql)
                .collect();
            assert!(
                ddl.contains("reconnect_notified_at_ms"),
                "migration 0024 embarquée"
            );
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

/// Credential relue sous le verrou, sans rien écrire.
fn peek(
    store: &PgConnectorTokenStore<InMemoryKeyProvider>,
    user: &str,
) -> Option<StoredCredential> {
    let mut seen = None;
    store
        .update_locked(user, "gmail", &mut |current| {
            seen = current;
            CredentialUpdate::Keep
        })
        .unwrap();
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn update_locked_replaces_invalidates_and_store_clears_the_mark() {
    let Some(pool) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let store = PgConnectorTokenStore::new(pool.clone(), cipher(3));
    // Absente : décision sans effet, aucune ligne créée.
    store
        .update_locked("frank", "gmail", &mut |current| {
            assert!(current.is_none());
            CredentialUpdate::Replace(tokens("at-ghost"))
        })
        .unwrap();
    assert_eq!(store.load("frank", "gmail").unwrap(), None);

    store.store("frank", "gmail", &tokens("at-1")).unwrap();
    store
        .update_locked("frank", "gmail", &mut |_| {
            CredentialUpdate::Replace(tokens("at-2"))
        })
        .unwrap();
    let seen = peek(&store, "frank").unwrap();
    assert_eq!(seen.tokens, tokens("at-2"));
    assert_eq!(seen.invalidated, None);

    let invalidation = CredentialInvalidation {
        at_ms: 1_700_000_000_000,
        reason: "invalid_grant".into(),
    };
    store
        .update_locked("frank", "gmail", &mut |_| {
            CredentialUpdate::Invalidate(invalidation.clone())
        })
        .unwrap();
    let seen = peek(&store, "frank").unwrap();
    assert_eq!(seen.invalidated, Some(invalidation.clone()));
    assert_eq!(seen.tokens, tokens("at-2"), "tokens conservés");

    // La contrainte refuse une marque à moitié posée.
    let half = pool
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE connector_tokens SET invalid_reason = NULL WHERE user_id = 'frank'",
            &[],
        )
        .await;
    assert!(half.is_err(), "CHECK connector_tokens_invalidation_check");

    // Reconnexion : marque effacée.
    store.store("frank", "gmail", &tokens("at-3")).unwrap();
    assert_eq!(peek(&store, "frank").unwrap().invalidated, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_locked_serializes_two_pools_on_the_row() {
    // Deux pools = deux processus : le second attend le verrou de ligne puis
    // voit l'écriture du premier.
    let Some(pool) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let a = std::sync::Arc::new(PgConnectorTokenStore::new(pool, cipher(4)));
    let b = std::sync::Arc::new(PgConnectorTokenStore::new(
        connect(&url).unwrap(),
        cipher(4),
    ));
    a.store("gina", "gmail", &tokens("at-0")).unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let run = |store: std::sync::Arc<PgConnectorTokenStore<InMemoryKeyProvider>>,
               barrier: std::sync::Arc<std::sync::Barrier>| {
        tokio::task::spawn_blocking(move || {
            barrier.wait();
            let mut seen = String::new();
            store
                .update_locked("gina", "gmail", &mut |current| {
                    seen = current.unwrap().tokens.access_token;
                    // « Appel au provider » pendant que le verrou est tenu.
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    let n: u32 = seen.trim_start_matches("at-").parse().unwrap();
                    CredentialUpdate::Replace(tokens(&format!("at-{}", n + 1)))
                })
                .unwrap();
            seen
        })
    };
    let (ra, rb) = (
        run(std::sync::Arc::clone(&a), std::sync::Arc::clone(&barrier)),
        run(std::sync::Arc::clone(&b), barrier),
    );
    let mut seen = [ra.await.unwrap(), rb.await.unwrap()];
    seen.sort();
    assert_eq!(
        seen,
        ["at-0", "at-1"],
        "le second a lu l'écriture du premier"
    );
    assert_eq!(
        b.load("gina", "gmail").unwrap().unwrap().access_token,
        "at-2"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn load_all_by_user_reads_tokens_and_marks_in_one_pass() {
    let Some(pool) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let store = PgConnectorTokenStore::new(pool, cipher(5));
    assert!(store.load_all_by_user("hugo").unwrap().is_empty());

    store.store("hugo", "slack", &tokens("xoxb-hugo")).unwrap();
    store.store("hugo", "gmail", &tokens("ya29-hugo")).unwrap();
    store.store("ines", "gmail", &tokens("ya29-ines")).unwrap();
    let invalidation = CredentialInvalidation {
        at_ms: 1_700_000_000_000,
        reason: "invalid_grant".into(),
    };
    store
        .update_locked("hugo", "gmail", &mut |_| {
            CredentialUpdate::Invalidate(invalidation.clone())
        })
        .unwrap();

    let hugo = store.load_all_by_user("hugo").unwrap();
    let providers: Vec<&str> = hugo.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(providers, ["gmail", "slack"], "tri par provider");
    assert_eq!(hugo[0].1.tokens, tokens("ya29-hugo"), "déchiffré");
    assert_eq!(hugo[0].1.invalidated, Some(invalidation));
    assert_eq!(hugo[1].1.invalidated, None);
    let ines = store.load_all_by_user("ines").unwrap();
    assert_eq!(ines.len(), 1, "isolé par utilisateur");
    assert_eq!(ines[0].1.invalidated, None);

    // Reconnexion : la marque disparaît aussi de la lecture groupée.
    store.store("hugo", "gmail", &tokens("ya29-new")).unwrap();
    let hugo = store.load_all_by_user("hugo").unwrap();
    assert_eq!(hugo[0].1.invalidated, None);
    assert_eq!(hugo[0].1.tokens, tokens("ya29-new"));
}

fn invalidate(store: &PgConnectorTokenStore<InMemoryKeyProvider>, user: &str, at_ms: i64) {
    store
        .update_locked(user, "slack", &mut |_| {
            CredentialUpdate::Invalidate(CredentialInvalidation {
                at_ms,
                reason: "invalid_grant".into(),
            })
        })
        .unwrap();
}

/// La réclamation porte sur toute la table : les tests qui réclament passent
/// l'un après l'autre.
static CLAIMS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Réclamations de ce test seulement : les autres tests du binaire posent
/// aussi des marques (plus récentes, `1_700_000_000_000`).
fn claimed_among(notices: Vec<ReconnectNotice>, users: &[&str]) -> Vec<(String, i64)> {
    notices
        .into_iter()
        .filter(|n| users.contains(&n.user_id.as_str()))
        .map(|n| (n.user_id, n.invalidated_at_ms))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_notices_are_claimed_once_and_again_after_a_new_invalidation() {
    let Some(pool) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let _claims = CLAIMS.lock().await;
    let store = PgConnectorTokenStore::new(pool, cipher(6));
    let users = ["jade", "karl", "lea"];
    for user in users {
        store.store(user, "slack", &tokens("xoxb")).unwrap();
    }
    // Dates minuscules : les plus anciennes de la table, réclamées d'abord.
    invalidate(&store, "karl", 2);
    invalidate(&store, "jade", 1);

    let first = store.claim_reconnect_notices(2).unwrap();
    assert_eq!(
        first,
        [
            ReconnectNotice {
                invalidated_at_ms: 1,
                user_id: "jade".into(),
                provider_id: "slack".into(),
            },
            ReconnectNotice {
                invalidated_at_ms: 2,
                user_id: "karl".into(),
                provider_id: "slack".into(),
            },
        ]
    );
    assert!(
        claimed_among(store.claim_reconnect_notices(1000).unwrap(), &users).is_empty(),
        "déjà réclamées ; lea, saine, jamais"
    );

    // Reconnexion puis nouvelle révocation : nouvelle notification.
    store.store("jade", "slack", &tokens("xoxb-new")).unwrap();
    assert!(claimed_among(store.claim_reconnect_notices(1000).unwrap(), &users).is_empty());
    invalidate(&store, "jade", 3);
    assert_eq!(
        claimed_among(store.claim_reconnect_notices(1000).unwrap(), &users),
        [("jade".to_string(), 3)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_pools_never_claim_the_same_reconnect_notice() {
    let Some(pool) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let _claims = CLAIMS.lock().await;
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let a = std::sync::Arc::new(PgConnectorTokenStore::new(pool, cipher(8)));
    let b = std::sync::Arc::new(PgConnectorTokenStore::new(
        connect(&url).unwrap(),
        cipher(8),
    ));
    let users: Vec<String> = (0..20).map(|i| format!("mona-{i:02}")).collect();
    for user in &users {
        a.store(user, "slack", &tokens("xoxb")).unwrap();
        invalidate(&a, user, 10);
    }

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let claim = |store: std::sync::Arc<PgConnectorTokenStore<InMemoryKeyProvider>>,
                 barrier: std::sync::Arc<std::sync::Barrier>| {
        tokio::task::spawn_blocking(move || {
            barrier.wait();
            let mut claimed = Vec::new();
            loop {
                let batch = store.claim_reconnect_notices(3).unwrap();
                if batch.is_empty() {
                    return claimed;
                }
                claimed.extend(batch);
            }
        })
    };
    let (ra, rb) = (
        claim(std::sync::Arc::clone(&a), std::sync::Arc::clone(&barrier)),
        claim(b, barrier),
    );
    let refs: Vec<&str> = users.iter().map(String::as_str).collect();
    let mut all = claimed_among(ra.await.unwrap(), &refs);
    all.extend(claimed_among(rb.await.unwrap(), &refs));
    all.sort();
    let expected: Vec<(String, i64)> = users.iter().map(|u| (u.clone(), 10)).collect();
    assert_eq!(
        all, expected,
        "chaque invalidation réclamée exactement une fois"
    );
}

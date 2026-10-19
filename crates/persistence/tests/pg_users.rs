//! Test d'intégration de [`PgUserStore`] contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). Cf. `pg_permission.rs`.

use std::collections::HashSet;
use std::sync::Arc;

use autome_core::{CreateUser, PermissionStore, UserStore};
use autome_persistence::{connect, PgPermissionStore, PgUserStore};
use tokio::sync::MutexGuard;

/// Les tests recréent les mêmes tables : exécution en série.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup() -> Option<(PgUserStore, PgPermissionStore, MutexGuard<'static, ()>)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let guard = SERIAL.lock().await;
    let pool = connect(&url).expect("pool");
    let client = pool.get().await.expect("connexion");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS user_permissions; \
             CREATE TABLE user_permissions (user_id TEXT NOT NULL, permission TEXT NOT NULL, \
               PRIMARY KEY (user_id, permission)); \
             DROP TABLE IF EXISTS users CASCADE; \
             CREATE TABLE users (id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
               email TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, \
               role TEXT NOT NULL DEFAULT 'user', \
               created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
               updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
               disabled_at TIMESTAMPTZ NULL, email_verified_at TIMESTAMPTZ NULL);",
        )
        .await
        .expect("schéma de test");
    Some((
        PgUserStore::new(pool.clone()),
        PgPermissionStore::new(pool),
        guard,
    ))
}

#[tokio::test]
async fn create_find_and_grant_atomically() {
    let Some((users, perms, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };

    let granted: HashSet<String> = ["workflows.read".to_string(), "workflows.write".to_string()]
        .into_iter()
        .collect();
    let CreateUser::Created(alice) = users
        .create("alice@example.io", "$argon2id$h1", &granted, true)
        .await
        .unwrap()
    else {
        panic!("compte attendu");
    };
    assert_eq!(alice.id.len(), 36, "id = UUID texte");
    assert_eq!(perms.permissions_for(&alice.id).await.unwrap(), granted);

    let again = users
        .create("alice@example.io", "$argon2id$h2", &granted, true)
        .await
        .unwrap();
    assert_eq!(again, CreateUser::EmailTaken);

    let found = users.find_by_email("alice@example.io").await.unwrap();
    assert_eq!(found, Some(alice.clone()));
    assert_eq!(
        users.find_by_email("nobody@example.io").await.unwrap(),
        None
    );

    // Changement de mot de passe (#196) : recherche par id, écriture
    // conditionnelle sur l'ancien hash.
    assert_eq!(
        users.find_by_id(&alice.id).await.unwrap().map(|u| u.email),
        Some("alice@example.io".to_string())
    );
    assert_eq!(
        users.find_by_id("alice").await.unwrap(),
        None,
        "id non UUID"
    );
    assert_eq!(
        users
            .find_by_id("00000000-0000-0000-0000-00000000dead")
            .await
            .unwrap(),
        None
    );
    assert!(!users
        .update_password(&alice.id, "$argon2id$périmé", "$argon2id$x")
        .await
        .unwrap());

    // 20 changements concurrents partis du même hash : un seul gagne.
    let users = Arc::new(users);
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let (users, id) = (Arc::clone(&users), alice.id.clone());
            tokio::spawn(async move {
                users
                    .update_password(&id, "$argon2id$h1", &format!("$argon2id$new-{i}"))
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut winners = 0;
    for t in tasks {
        winners += usize::from(t.await.unwrap());
    }
    assert_eq!(winners, 1);
    let stored = users.find_by_id(&alice.id).await.unwrap().unwrap();
    assert!(stored.password_hash.starts_with("$argon2id$new-"));
}

fn editor() -> HashSet<String> {
    ["workflows.read".to_string(), "workflows.write".to_string()]
        .into_iter()
        .collect()
}

async fn created(users: &PgUserStore, email: &str) -> autome_core::UserRecord {
    match users
        .create(email, "$argon2id$h", &editor(), true)
        .await
        .unwrap()
    {
        CreateUser::Created(record) => record,
        CreateUser::EmailTaken => panic!("email déjà pris"),
    }
}

#[tokio::test]
async fn admin_list_filter_and_pagination() {
    let Some((users, _perms, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    for email in ["carol@example.io", "alice@example.io", "bob_x@other.io"] {
        created(&users, email).await;
    }

    let all = users.list(None, 0, 10).await.unwrap();
    assert_eq!(all.total, 3);
    let emails: Vec<&str> = all.items.iter().map(|u| u.email.as_str()).collect();
    assert_eq!(
        emails,
        ["carol@example.io", "alice@example.io", "bob_x@other.io"],
        "ordre de création"
    );
    assert_eq!(all.items[0].permissions, editor());
    assert!(!all.items[0].disabled);
    assert!(
        all.items[0].created_at_ms > 1_600_000_000_000,
        "millisecondes Unix"
    );

    let page = users.list(None, 1, 1).await.unwrap();
    assert_eq!((page.total, page.items.len()), (3, 1));
    assert_eq!(page.items[0].email, "alice@example.io");
    assert!(users.list(None, 10, 10).await.unwrap().items.is_empty());

    let filtered = users.list(Some("example.io"), 0, 10).await.unwrap();
    assert_eq!(filtered.total, 2);
    // `_` et `%` sont des caractères littéraux, pas des jokers.
    assert_eq!(users.list(Some("b_x"), 0, 10).await.unwrap().total, 1);
    assert_eq!(users.list(Some("o_x"), 0, 10).await.unwrap().total, 0);
    assert_eq!(users.list(Some("%"), 0, 10).await.unwrap().total, 0);
}

#[tokio::test]
async fn admin_permissions_replacement_and_suspension() {
    let Some((users, perms, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let alice = created(&users, "alice@example.io").await;
    let viewer: HashSet<String> = ["workflows.read".to_string()].into_iter().collect();

    assert!(users.set_permissions(&alice.id, &viewer).await.unwrap());
    assert_eq!(perms.permissions_for(&alice.id).await.unwrap(), viewer);
    let none = HashSet::new();
    assert!(users.set_permissions(&alice.id, &none).await.unwrap());
    let summary = users.summary(&alice.id).await.unwrap().unwrap();
    assert!(summary.permissions.is_empty(), "ARRAY vide ⇒ ensemble vide");
    assert!(!users.set_permissions("alice", &viewer).await.unwrap());
    assert!(!users
        .set_permissions("00000000-0000-0000-0000-00000000dead", &viewer)
        .await
        .unwrap());
    assert!(perms
        .permissions_for("00000000-0000-0000-0000-00000000dead")
        .await
        .unwrap()
        .is_empty());

    // Remplacements concurrents : l'état final est l'un des jeux, jamais un mélange.
    let users = Arc::new(users);
    let sets: Vec<HashSet<String>> = (0..10)
        .map(|i| [format!("p{i}.a"), format!("p{i}.b")].into_iter().collect())
        .collect();
    let tasks: Vec<_> = sets
        .iter()
        .cloned()
        .map(|set| {
            let (users, id) = (Arc::clone(&users), alice.id.clone());
            tokio::spawn(async move { users.set_permissions(&id, &set).await.unwrap() })
        })
        .collect();
    for t in tasks {
        assert!(t.await.unwrap());
    }
    let last = perms.permissions_for(&alice.id).await.unwrap();
    assert!(sets.contains(&last), "jeu mélangé : {last:?}");

    // Suspension : visible des recherches, idempotente, réversible.
    assert!(users.set_disabled(&alice.id, true).await.unwrap());
    assert!(users.set_disabled(&alice.id, true).await.unwrap());
    assert!(users.find_by_id(&alice.id).await.unwrap().unwrap().disabled);
    assert!(
        users
            .find_by_email("alice@example.io")
            .await
            .unwrap()
            .unwrap()
            .disabled
    );
    assert!(users.summary(&alice.id).await.unwrap().unwrap().disabled);
    assert!(users.set_disabled(&alice.id, false).await.unwrap());
    assert!(!users.find_by_id(&alice.id).await.unwrap().unwrap().disabled);
    assert!(!users.set_disabled("alice", true).await.unwrap());
    assert_eq!(users.summary("alice").await.unwrap(), None);
}

#[tokio::test]
async fn email_verification_is_conditional_on_the_address() {
    let Some((users, _perms, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let CreateUser::Created(alice) = users
        .create("alice@example.io", "$argon2id$h", &editor(), false)
        .await
        .unwrap()
    else {
        panic!("compte attendu");
    };
    assert!(!alice.email_verified);
    assert!(
        !users
            .find_by_id(&alice.id)
            .await
            .unwrap()
            .unwrap()
            .email_verified
    );
    assert!(
        !users
            .summary(&alice.id)
            .await
            .unwrap()
            .unwrap()
            .email_verified
    );

    // Autre adresse, compte inconnu, id non UUID : rien n'est écrit.
    assert!(!users
        .mark_email_verified(&alice.id, "old@example.io")
        .await
        .unwrap());
    assert!(!users
        .mark_email_verified("00000000-0000-0000-0000-00000000dead", "alice@example.io")
        .await
        .unwrap());
    assert!(!users
        .mark_email_verified("alice", "alice@example.io")
        .await
        .unwrap());
    assert!(
        !users
            .find_by_id(&alice.id)
            .await
            .unwrap()
            .unwrap()
            .email_verified
    );

    assert!(users
        .mark_email_verified(&alice.id, "alice@example.io")
        .await
        .unwrap());
    assert!(users
        .mark_email_verified(&alice.id, "alice@example.io")
        .await
        .unwrap());
    assert!(
        users
            .find_by_email("alice@example.io")
            .await
            .unwrap()
            .unwrap()
            .email_verified
    );
    let page = users.list(None, 0, 10).await.unwrap();
    assert!(page.items[0].email_verified);

    let bob = created(&users, "bob@example.io").await;
    assert!(bob.email_verified, "créé vérifié");
    assert!(
        users
            .find_by_id(&bob.id)
            .await
            .unwrap()
            .unwrap()
            .email_verified
    );
}

#[tokio::test]
async fn an_invited_account_has_no_password_until_it_chooses_one() {
    let Some((users, _perms, _serial)) = setup().await else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    let invited = autome_core::unusable_password_hash();
    let CreateUser::Created(alice) = users
        .create("alice@example.io", &invited, &editor(), false)
        .await
        .unwrap()
    else {
        panic!("compte attendu");
    };
    created(&users, "bob@example.io").await;
    let page = users.list(None, 0, 10).await.unwrap();
    assert!(!page.items[0].password_set, "invitation en attente");
    assert!(page.items[1].password_set);
    assert!(
        !users
            .summary(&alice.id)
            .await
            .unwrap()
            .unwrap()
            .password_set
    );

    // Renvoi : nouveau hash inutilisable, l'ancien ne vaut plus.
    let resent = autome_core::unusable_password_hash();
    assert!(users
        .update_password(&alice.id, &invited, &resent)
        .await
        .unwrap());
    assert!(!users
        .update_password(&alice.id, &invited, "$argon2id$x")
        .await
        .unwrap());
    // Acceptation.
    assert!(users
        .update_password(&alice.id, &resent, "$argon2id$chosen")
        .await
        .unwrap());
    assert!(
        users
            .summary(&alice.id)
            .await
            .unwrap()
            .unwrap()
            .password_set
    );
}

//! Comptes utilisateurs (backlog post-audit — auth P0).
//!
//! Le contrat [`UserStore`] persiste un compte `(email, hash PHC)` et lui
//! attribue ses permissions **dans la même opération** : un compte créé sans
//! ses permissions (ou l'inverse) serait un état incohérent. L'impl PG le
//! garantit par une transaction ; [`InMemoryUserStore`] écrit dans le
//! [`InMemoryPermissionStore`] partagé avec l'`AuthState`.
//!
//! L'email est la clé de connexion : il est **normalisé** ([`normalize_email`])
//! avant toute écriture ou recherche, pour que `Alice@X.io` et `alice@x.io`
//! désignent le même compte. Les règles de saisie ([`validate_email`],
//! [`validate_password`]) vivent ici pour être partagées par tous les points
//! d'entrée (API, CLI d'admin éventuelle).
//!
//! L'administration des comptes (#208) passe aussi par ce contrat : liste
//! paginée ([`UserStore::list`]), remplacement des permissions
//! ([`UserStore::set_permissions`], atomique) et suspension
//! ([`UserStore::set_disabled`]). Un compte suspendu garde ses données et ses
//! permissions ; c'est l'API qui refuse sa connexion et ferme ses sessions.
//!
//! La vérification de l'email (#231) n'est qu'un drapeau : le compte est créé
//! vérifié ou non (selon que le déploiement exige la preuve de possession de
//! l'adresse), puis [`UserStore::mark_email_verified`] le passe à vérifié.
//! C'est l'API qui refuse la connexion d'un compte non vérifié.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rand::RngCore;

use crate::rbac::{InMemoryPermissionStore, PermissionStore};
use crate::{Error, Result};

/// Longueur minimale d'un mot de passe (alignée sur le frontend, `auth.ts`).
pub const MIN_PASSWORD_LENGTH: usize = 8;
/// Longueur maximale acceptée : borne le coût Argon2 d'une requête hostile.
pub const MAX_PASSWORD_LENGTH: usize = 1024;
/// Longueur maximale d'une adresse email (RFC 5321).
pub const MAX_EMAIL_LENGTH: usize = 254;

/// Compte utilisateur tel que stocké.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    /// Identifiant stable — porté par la claim `sub` des JWT.
    pub id: String,
    /// Email normalisé.
    pub email: String,
    /// Hash PHC du mot de passe (cf. [`crate::PasswordHasher`]).
    pub password_hash: String,
    /// Compte suspendu par un administrateur (#208) : connexion refusée.
    pub disabled: bool,
    /// Adresse email vérifiée (#231) : possession prouvée par un lien reçu.
    pub email_verified: bool,
}

/// Vue d'administration d'un compte (#208) — jamais le hash du mot de passe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSummary {
    /// Identifiant stable (`claims.sub`).
    pub id: String,
    /// Email normalisé.
    pub email: String,
    /// Compte suspendu.
    pub disabled: bool,
    /// Adresse email vérifiée (#231).
    pub email_verified: bool,
    /// Création du compte, en millisecondes Unix.
    pub created_at_ms: i64,
    /// Permissions accordées.
    pub permissions: HashSet<String>,
}

/// Page de [`UserStore::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPage {
    /// Comptes de la page, du plus ancien au plus récent.
    pub items: Vec<UserSummary>,
    /// Nombre total de comptes correspondant au filtre (hors pagination).
    pub total: usize,
}

/// Issue d'une création de compte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateUser {
    /// Compte créé.
    Created(UserRecord),
    /// Un compte existe déjà pour cet email (rien n'a été écrit).
    EmailTaken,
}

/// Persistance des comptes utilisateurs.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// Crée le compte et lui accorde `permissions`, atomiquement.
    /// `email` doit déjà être normalisé ; `email_verified` : adresse tenue
    /// pour vérifiée dès la création (#231).
    async fn create(
        &self,
        email: &str,
        password_hash: &str,
        permissions: &HashSet<String>,
        email_verified: bool,
    ) -> Result<CreateUser>;

    /// Recherche un compte par email normalisé.
    async fn find_by_email(&self, email: &str) -> Result<Option<UserRecord>>;

    /// Recherche un compte par identifiant (`claims.sub`). `None` si inconnu,
    /// y compris pour un identifiant mal formé.
    async fn find_by_id(&self, id: &str) -> Result<Option<UserRecord>>;

    /// Remplace le hash du mot de passe de `id` **seulement** s'il vaut encore
    /// `expected_hash` (celui qu'on vient de vérifier) : de deux changements
    /// concurrents, un seul gagne (#196). `false` si le compte est inconnu ou
    /// si le mot de passe a changé entre-temps.
    async fn update_password(&self, id: &str, expected_hash: &str, new_hash: &str) -> Result<bool>;

    /// Comptes triés par date de création (puis id), filtrés par `query` :
    /// sous-chaîne de l'email, à passer déjà normalisée (#208).
    async fn list(&self, query: Option<&str>, offset: usize, limit: usize) -> Result<UserPage>;

    /// Vue d'administration d'un compte, permissions comprises. `None` si
    /// inconnu, y compris pour un identifiant mal formé.
    async fn summary(&self, id: &str) -> Result<Option<UserSummary>>;

    /// Remplace **toutes** les permissions de `id`, atomiquement (aucun état
    /// intermédiaire visible). `false` si le compte est inconnu.
    async fn set_permissions(&self, id: &str, permissions: &HashSet<String>) -> Result<bool>;

    /// Suspend (`true`) ou réactive (`false`) le compte. Idempotent ; `false`
    /// si le compte est inconnu.
    async fn set_disabled(&self, id: &str, disabled: bool) -> Result<bool>;

    /// Marque l'adresse de `id` vérifiée (#231) **seulement** si elle vaut
    /// encore `email` : un lien émis pour une autre adresse ne vérifie rien.
    /// Idempotent ; `false` si le compte est inconnu ou son email différent.
    async fn mark_email_verified(&self, id: &str, email: &str) -> Result<bool>;
}

/// Normalise un email : espaces de bord retirés, minuscules.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Valide un email **déjà normalisé**. Volontairement simple (une partie
/// locale, un `@`, un domaine avec un point, pas d'espace) : la seule preuve
/// qu'une adresse existe reste un email de confirmation.
pub fn validate_email(email: &str) -> Result<()> {
    let invalid = || Error::Invalid("email invalide".into());
    if email.len() > MAX_EMAIL_LENGTH || email.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let (local, domain) = email.split_once('@').ok_or_else(invalid)?;
    let domain_ok = !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.');
    if local.is_empty() || !domain_ok {
        return Err(invalid());
    }
    Ok(())
}

/// Valide la politique de mot de passe (longueur en caractères).
pub fn validate_password(password: &str) -> Result<()> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LENGTH {
        return Err(Error::Invalid(format!(
            "mot de passe trop court (minimum {MIN_PASSWORD_LENGTH} caractères)"
        )));
    }
    if len > MAX_PASSWORD_LENGTH {
        return Err(Error::Invalid(format!(
            "mot de passe trop long (maximum {MAX_PASSWORD_LENGTH} caractères)"
        )));
    }
    Ok(())
}

/// Identifiant aléatoire au format UUID (v4) — même forme que les ids
/// générés par PostgreSQL (`gen_random_uuid()`).
fn fresh_user_id() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Compte en mémoire : l'enregistrement et son ordre de création.
struct StoredUser {
    record: UserRecord,
    created_at_ms: i64,
    /// Rang de création : départage les comptes créés la même milliseconde.
    seq: u64,
}

/// Impl in-process (dev, tests, single-node). Les permissions sont écrites
/// dans le [`InMemoryPermissionStore`] fourni — le même que celui de
/// l'`AuthState`, sans quoi le compte créé n'aurait aucun droit effectif.
pub struct InMemoryUserStore {
    /// Comptes par email normalisé.
    users: Mutex<HashMap<String, StoredUser>>,
    permissions: Arc<InMemoryPermissionStore>,
}

impl InMemoryUserStore {
    /// Construit un store vide adossé à `permissions`.
    pub fn new(permissions: Arc<InMemoryPermissionStore>) -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            permissions,
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, StoredUser>>> {
        self.users
            .lock()
            .map_err(|_| Error::Invalid("users lock empoisonné".into()))
    }

    /// Supprime le compte `id` et ses permissions (suppression de compte,
    /// #229) ; renvoie l'enregistrement supprimé, `None` si inconnu.
    pub fn remove(&self, id: &str) -> Result<Option<UserRecord>> {
        let mut users = self.lock()?;
        let Some(email) = users
            .iter()
            .find(|(_, u)| u.record.id == id)
            .map(|(email, _)| email.clone())
        else {
            return Ok(None);
        };
        self.permissions.replace(id, &HashSet::new())?;
        Ok(users.remove(&email).map(|u| u.record))
    }

    /// Complète des comptes de leurs permissions (verrou des comptes relâché).
    async fn summaries(&self, users: Vec<(UserRecord, i64)>) -> Result<Vec<UserSummary>> {
        let mut out = Vec::with_capacity(users.len());
        for (record, created_at_ms) in users {
            out.push(UserSummary {
                permissions: self.permissions.permissions_for(&record.id).await?,
                id: record.id,
                email: record.email,
                disabled: record.disabled,
                email_verified: record.email_verified,
                created_at_ms,
            });
        }
        Ok(out)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl UserStore for InMemoryUserStore {
    async fn create(
        &self,
        email: &str,
        password_hash: &str,
        permissions: &HashSet<String>,
        email_verified: bool,
    ) -> Result<CreateUser> {
        let mut users = self.lock()?;
        if users.contains_key(email) {
            return Ok(CreateUser::EmailTaken);
        }
        let record = UserRecord {
            id: fresh_user_id(),
            email: email.to_string(),
            password_hash: password_hash.to_string(),
            disabled: false,
            email_verified,
        };
        for p in permissions {
            self.permissions.grant(&record.id, p.clone())?;
        }
        let seq = users.len() as u64;
        users.insert(
            email.to_string(),
            StoredUser {
                record: record.clone(),
                created_at_ms: now_ms(),
                seq,
            },
        );
        Ok(CreateUser::Created(record))
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<UserRecord>> {
        Ok(self.lock()?.get(email).map(|u| u.record.clone()))
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<UserRecord>> {
        Ok(self
            .lock()?
            .values()
            .find(|u| u.record.id == id)
            .map(|u| u.record.clone()))
    }

    async fn update_password(&self, id: &str, expected_hash: &str, new_hash: &str) -> Result<bool> {
        let mut users = self.lock()?;
        match users.values_mut().find(|u| u.record.id == id) {
            Some(user) if user.record.password_hash == expected_hash => {
                user.record.password_hash = new_hash.to_string();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn list(&self, query: Option<&str>, offset: usize, limit: usize) -> Result<UserPage> {
        let (page, total) = {
            let users = self.lock()?;
            let mut matching: Vec<&StoredUser> = users
                .values()
                .filter(|u| query.is_none_or(|q| u.record.email.contains(q)))
                .collect();
            matching.sort_by_key(|u| (u.created_at_ms, u.seq));
            let total = matching.len();
            let page: Vec<(UserRecord, i64)> = matching
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|u| (u.record.clone(), u.created_at_ms))
                .collect();
            (page, total)
        };
        Ok(UserPage {
            items: self.summaries(page).await?,
            total,
        })
    }

    async fn summary(&self, id: &str) -> Result<Option<UserSummary>> {
        let found = self
            .lock()?
            .values()
            .find(|u| u.record.id == id)
            .map(|u| (u.record.clone(), u.created_at_ms));
        match found {
            Some(user) => Ok(self.summaries(vec![user]).await?.pop()),
            None => Ok(None),
        }
    }

    async fn set_permissions(&self, id: &str, permissions: &HashSet<String>) -> Result<bool> {
        // Verrou des comptes tenu pendant l'écriture : deux remplacements
        // concurrents ne se mélangent pas.
        let users = self.lock()?;
        if !users.values().any(|u| u.record.id == id) {
            return Ok(false);
        }
        self.permissions.replace(id, permissions)?;
        Ok(true)
    }

    async fn set_disabled(&self, id: &str, disabled: bool) -> Result<bool> {
        let mut users = self.lock()?;
        match users.values_mut().find(|u| u.record.id == id) {
            Some(user) => {
                user.record.disabled = disabled;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn mark_email_verified(&self, id: &str, email: &str) -> Result<bool> {
        let mut users = self.lock()?;
        match users.get_mut(email) {
            Some(user) if user.record.id == id => {
                user.record.email_verified = true;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbac::Role;

    fn store() -> (InMemoryUserStore, Arc<InMemoryPermissionStore>) {
        let perms = Arc::new(InMemoryPermissionStore::new());
        (InMemoryUserStore::new(Arc::clone(&perms)), perms)
    }

    #[test]
    fn normalize_trims_and_lowercases() {
        assert_eq!(normalize_email("  Alice@Example.IO "), "alice@example.io");
    }

    #[test]
    fn validate_email_accepts_plain_address() {
        assert!(validate_email("alice@example.io").is_ok());
        assert!(validate_email("a.b+tag@sub.example.io").is_ok());
    }

    #[test]
    fn validate_email_rejects_malformed() {
        for bad in [
            "",
            "alice",
            "@example.io",
            "alice@",
            "alice@localhost",
            "alice@.io",
            "alice@example.",
            "al ice@example.io",
            "a@b@example.io",
        ] {
            assert!(validate_email(bad).is_err(), "devrait refuser {bad:?}");
        }
        let too_long = format!("{}@example.io", "a".repeat(MAX_EMAIL_LENGTH));
        assert!(validate_email(&too_long).is_err());
    }

    #[test]
    fn validate_password_enforces_bounds_in_chars() {
        assert!(validate_password("1234567").is_err());
        assert!(validate_password("12345678").is_ok());
        // 8 caractères multi-octets : la borne compte des caractères.
        assert!(validate_password("éééééééé").is_ok());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_LENGTH)).is_ok());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_LENGTH + 1)).is_err());
    }

    #[test]
    fn fresh_ids_are_uuid_shaped_and_unique() {
        let a = fresh_user_id();
        let b = fresh_user_id();
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
        assert_eq!(&a[14..15], "4", "version 4");
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn create_then_find_and_grant_permissions() {
        let (users, perms) = store();
        let role = Role::editor();
        let created = users
            .create(
                "alice@example.io",
                "$argon2id$hash",
                &role.permissions,
                true,
            )
            .await
            .unwrap();
        let CreateUser::Created(record) = created else {
            panic!("compte attendu");
        };
        assert_eq!(record.email, "alice@example.io");

        let found = users.find_by_email("alice@example.io").await.unwrap();
        assert_eq!(found, Some(record.clone()));
        assert_eq!(
            perms.permissions_for(&record.id).await.unwrap(),
            role.permissions
        );
    }

    #[tokio::test]
    async fn duplicate_email_is_reported_without_overwrite() {
        let (users, _) = store();
        let none = HashSet::new();
        users
            .create("bob@example.io", "h1", &none, true)
            .await
            .unwrap();
        let again = users
            .create("bob@example.io", "h2", &none, true)
            .await
            .unwrap();
        assert_eq!(again, CreateUser::EmailTaken);
        let kept = users
            .find_by_email("bob@example.io")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.password_hash, "h1");
    }

    #[tokio::test]
    async fn unknown_email_is_none() {
        let (users, _) = store();
        assert_eq!(users.find_by_email("x@example.io").await.unwrap(), None);
    }

    async fn created(users: &InMemoryUserStore, email: &str) -> UserRecord {
        match users
            .create(email, "hash", &Role::editor().permissions, true)
            .await
            .unwrap()
        {
            CreateUser::Created(record) => record,
            CreateUser::EmailTaken => panic!("email déjà pris"),
        }
    }

    #[tokio::test]
    async fn list_is_ordered_by_creation_filtered_and_paginated() {
        let (users, _) = store();
        for email in ["carol@example.io", "alice@example.io", "bob@other.io"] {
            created(&users, email).await;
        }
        let all = users.list(None, 0, 10).await.unwrap();
        assert_eq!(all.total, 3);
        let emails: Vec<&str> = all.items.iter().map(|u| u.email.as_str()).collect();
        assert_eq!(
            emails,
            ["carol@example.io", "alice@example.io", "bob@other.io"],
            "ordre de création"
        );
        assert_eq!(all.items[0].permissions, Role::editor().permissions);
        assert!(!all.items[0].disabled);

        let page = users.list(None, 1, 1).await.unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].email, "alice@example.io");

        let filtered = users.list(Some("example.io"), 0, 10).await.unwrap();
        assert_eq!(filtered.total, 2);
        assert!(users
            .list(Some("nobody"), 0, 10)
            .await
            .unwrap()
            .items
            .is_empty());
        assert!(users.list(None, 5, 10).await.unwrap().items.is_empty());
    }

    #[tokio::test]
    async fn set_permissions_replaces_the_whole_set() {
        let (users, perms) = store();
        let alice = created(&users, "alice@example.io").await;
        assert!(users
            .set_permissions(&alice.id, &Role::viewer().permissions)
            .await
            .unwrap());
        assert_eq!(
            perms.permissions_for(&alice.id).await.unwrap(),
            Role::viewer().permissions
        );
        assert_eq!(
            users.summary(&alice.id).await.unwrap().unwrap().permissions,
            Role::viewer().permissions
        );
        assert!(!users
            .set_permissions("inconnu", &Role::viewer().permissions)
            .await
            .unwrap());
        assert!(perms.permissions_for("inconnu").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_disabled_is_reported_by_lookups() {
        let (users, _) = store();
        let alice = created(&users, "alice@example.io").await;
        assert!(users.set_disabled(&alice.id, true).await.unwrap());
        assert!(
            users.set_disabled(&alice.id, true).await.unwrap(),
            "idempotent"
        );
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
        assert!(!users.set_disabled("inconnu", true).await.unwrap());
        assert_eq!(users.summary("inconnu").await.unwrap(), None);
    }

    #[tokio::test]
    async fn find_by_id_and_conditional_password_update() {
        let (users, _) = store();
        let CreateUser::Created(alice) = users
            .create("alice@example.io", "hash-v1", &HashSet::new(), true)
            .await
            .unwrap()
        else {
            panic!("création");
        };
        assert_eq!(
            users.find_by_id(&alice.id).await.unwrap(),
            Some(alice.clone())
        );
        assert_eq!(users.find_by_id("inconnu").await.unwrap(), None);

        // Hash attendu périmé : rien n'est écrit.
        assert!(!users
            .update_password(&alice.id, "hash-v0", "hash-x")
            .await
            .unwrap());
        assert!(users
            .update_password(&alice.id, "hash-v1", "hash-v2")
            .await
            .unwrap());
        // Le second de deux changements partis du même hash échoue.
        assert!(!users
            .update_password(&alice.id, "hash-v1", "hash-v3")
            .await
            .unwrap());
        let stored = users
            .find_by_email("alice@example.io")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.password_hash, "hash-v2");
        assert!(!users
            .update_password("inconnu", "hash-v2", "hash-x")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn email_verification_is_set_at_creation_or_marked_later() {
        let (users, _) = store();
        let none = HashSet::new();
        let CreateUser::Created(alice) = users
            .create("alice@example.io", "h", &none, false)
            .await
            .unwrap()
        else {
            panic!("création");
        };
        assert!(!alice.email_verified);
        assert!(
            !users
                .summary(&alice.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );

        // Lien émis pour une autre adresse, ou compte inconnu : rien n'est écrit.
        assert!(!users
            .mark_email_verified(&alice.id, "old@example.io")
            .await
            .unwrap());
        assert!(!users
            .mark_email_verified("inconnu", "alice@example.io")
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
        assert!(
            users
                .mark_email_verified(&alice.id, "alice@example.io")
                .await
                .unwrap(),
            "idempotent"
        );
        assert!(
            users
                .find_by_email("alice@example.io")
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        assert!(
            users
                .summary(&alice.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );

        let CreateUser::Created(bob) = users
            .create("bob@example.io", "h", &none, true)
            .await
            .unwrap()
        else {
            panic!("création");
        };
        assert!(bob.email_verified, "créé vérifié");
    }

    #[tokio::test]
    async fn remove_deletes_the_account_and_its_permissions() {
        let (users, perms) = store();
        let alice = created(&users, "alice@example.io").await;
        let bob = created(&users, "bob@example.io").await;
        assert_eq!(users.remove(&alice.id).unwrap(), Some(alice.clone()));
        assert_eq!(users.find_by_id(&alice.id).await.unwrap(), None);
        assert_eq!(users.find_by_email("alice@example.io").await.unwrap(), None);
        assert!(perms.permissions_for(&alice.id).await.unwrap().is_empty());
        assert_eq!(users.remove(&alice.id).unwrap(), None);
        assert!(users.find_by_id(&bob.id).await.unwrap().is_some());
        // L'email redevient libre.
        assert!(matches!(
            users
                .create("alice@example.io", "h", &HashSet::new(), true)
                .await
                .unwrap(),
            CreateUser::Created(_)
        ));
    }
}

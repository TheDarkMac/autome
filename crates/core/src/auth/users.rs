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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rand::RngCore;

use crate::rbac::InMemoryPermissionStore;
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
    /// `email` doit déjà être normalisé.
    async fn create(
        &self,
        email: &str,
        password_hash: &str,
        permissions: &HashSet<String>,
    ) -> Result<CreateUser>;

    /// Recherche un compte par email normalisé.
    async fn find_by_email(&self, email: &str) -> Result<Option<UserRecord>>;
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

/// Impl in-process (dev, tests, single-node). Les permissions sont écrites
/// dans le [`InMemoryPermissionStore`] fourni — le même que celui de
/// l'`AuthState`, sans quoi le compte créé n'aurait aucun droit effectif.
pub struct InMemoryUserStore {
    users: Mutex<HashMap<String, UserRecord>>,
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
}

#[async_trait]
impl UserStore for InMemoryUserStore {
    async fn create(
        &self,
        email: &str,
        password_hash: &str,
        permissions: &HashSet<String>,
    ) -> Result<CreateUser> {
        let mut users = self
            .users
            .lock()
            .map_err(|_| Error::Invalid("users lock empoisonné".into()))?;
        if users.contains_key(email) {
            return Ok(CreateUser::EmailTaken);
        }
        let record = UserRecord {
            id: fresh_user_id(),
            email: email.to_string(),
            password_hash: password_hash.to_string(),
        };
        for p in permissions {
            self.permissions.grant(&record.id, p.clone())?;
        }
        users.insert(email.to_string(), record.clone());
        Ok(CreateUser::Created(record))
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<UserRecord>> {
        let users = self
            .users
            .lock()
            .map_err(|_| Error::Invalid("users lock empoisonné".into()))?;
        Ok(users.get(email).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbac::{PermissionStore, Role};

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
            .create("alice@example.io", "$argon2id$hash", &role.permissions)
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
        users.create("bob@example.io", "h1", &none).await.unwrap();
        let again = users.create("bob@example.io", "h2", &none).await.unwrap();
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
}

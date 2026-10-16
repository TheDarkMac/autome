//! Contrôle d'accès basé sur les rôles (F6.5).
//!
//! Modèle minimal :
//!
//! - [`Permission`] : chaîne typée, ex. `"workflows.write"`. Constantes
//!   [`PERM_WORKFLOWS_READ`], [`PERM_USERS_ADMIN`], … pour les permissions
//!   standard.
//! - [`Role`] : nom + ensemble de permissions. Pas de hiérarchie native ;
//!   un super-rôle = un rôle qui contient le wildcard `"*"`.
//! - [`PermissionStore`] : trait qui résout `user_id → HashSet<Permission>`.
//!   Impl `InMemoryPermissionStore` pour tests et mode dev.
//! - [`has_permission`] : fonction de check, supporte le wildcard `"*"`.
//!
//! L'**enforcement** (mapping vers 403, intégration axum) vit dans
//! `autome_api::auth`. La couche core reste agnostique du framework.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Wildcard reconnu par [`has_permission`] : un user qui le possède passe
/// tous les checks. À réserver aux super-admins.
pub const PERM_WILDCARD: &str = "*";

/// Lecture des workflows (CRUD lecture seule).
pub const PERM_WORKFLOWS_READ: &str = "workflows.read";
/// Création/modification/suppression de workflows.
pub const PERM_WORKFLOWS_WRITE: &str = "workflows.write";
/// Démarrage manuel d'un run.
pub const PERM_RUNS_EXECUTE: &str = "runs.execute";
/// Lecture des runs (historique, logs).
pub const PERM_RUNS_READ: &str = "runs.read";
/// Gestion des credentials connecteurs (création, rotation).
pub const PERM_CREDENTIALS_MANAGE: &str = "credentials.manage";
/// Administration des utilisateurs (création, attribution de rôles).
pub const PERM_USERS_ADMIN: &str = "users.admin";

/// Identifiant typé d'une permission. Volontairement opaque pour éviter
/// de confondre une `String` quelconque avec une permission RBAC.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Permission(String);

impl Permission {
    /// Construit une permission à partir de n'importe quel libellé. Vide → erreur.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let s = name.into();
        if s.is_empty() {
            return Err(Error::Invalid("permission : libellé vide".into()));
        }
        Ok(Self(s))
    }

    /// Vue chaîne.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Permission {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for Permission {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Un rôle : un libellé + un ensemble de permissions.
///
/// Les rôles sont une commodité de management (« attribuer le rôle
/// `editor` à un utilisateur »). Le check côté trait
/// [`PermissionStore::permissions_for`] travaille toujours sur des
/// permissions, pas des rôles — ce qui simplifie la mise en cache et le
/// passage à un store distant (DB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    /// Nom du rôle (ex. `"admin"`, `"editor"`, `"viewer"`).
    pub name: String,
    /// Permissions accordées par ce rôle.
    pub permissions: HashSet<String>,
}

impl Role {
    /// Construit un rôle vide.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            permissions: HashSet::new(),
        }
    }

    /// Builder : ajoute des permissions.
    pub fn with_permissions<I, S>(mut self, perms: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.permissions.extend(perms.into_iter().map(Into::into));
        self
    }

    /// Convenience : rôle `admin` avec wildcard.
    pub fn super_admin() -> Self {
        Self::new(ROLE_ADMIN).with_permissions([PERM_WILDCARD])
    }

    /// Convenience : rôle `editor` (CRUD workflows + run + credentials).
    pub fn editor() -> Self {
        Self::new(ROLE_EDITOR).with_permissions([
            PERM_WORKFLOWS_READ,
            PERM_WORKFLOWS_WRITE,
            PERM_RUNS_EXECUTE,
            PERM_RUNS_READ,
            PERM_CREDENTIALS_MANAGE,
        ])
    }

    /// Convenience : rôle `viewer` (lecture seule).
    pub fn viewer() -> Self {
        Self::new(ROLE_VIEWER).with_permissions([PERM_WORKFLOWS_READ, PERM_RUNS_READ])
    }

    /// Rôle standard attribuable par l'administration (#208) : `admin`,
    /// `editor` ou `viewer`. `None` pour tout autre nom.
    pub fn standard(name: &str) -> Option<Self> {
        match name {
            ROLE_ADMIN => Some(Self::super_admin()),
            ROLE_EDITOR => Some(Self::editor()),
            ROLE_VIEWER => Some(Self::viewer()),
            _ => None,
        }
    }
}

/// Nom du rôle super-admin (wildcard `*`).
pub const ROLE_ADMIN: &str = "admin";
/// Nom du rôle éditeur (attribué à l'inscription).
pub const ROLE_EDITOR: &str = "editor";
/// Nom du rôle lecteur.
pub const ROLE_VIEWER: &str = "viewer";
/// Rôle affiché pour un jeu de permissions qui ne correspond à aucun rôle
/// standard (permissions accordées une à une).
pub const ROLE_CUSTOM: &str = "custom";

/// Rôle standard correspondant exactement à `permissions` : `admin` dès que
/// le wildcard est présent, [`ROLE_CUSTOM`] si aucun rôle ne correspond.
/// Les permissions restent la source de vérité ; le rôle n'est qu'une vue.
pub fn role_of(permissions: &HashSet<String>) -> &'static str {
    if permissions.contains(PERM_WILDCARD) {
        ROLE_ADMIN
    } else if *permissions == Role::editor().permissions {
        ROLE_EDITOR
    } else if *permissions == Role::viewer().permissions {
        ROLE_VIEWER
    } else {
        ROLE_CUSTOM
    }
}

/// Retourne `true` si `granted` contient `needed`, ou contient le
/// wildcard `"*"`.
pub fn has_permission(granted: &HashSet<String>, needed: &str) -> bool {
    granted.contains(PERM_WILDCARD) || granted.contains(needed)
}

// ────────────────────── PermissionStore ──────────────────────

/// Résolution `user_id → permissions`.
///
/// Une impl peut résoudre à travers des rôles intermédiaires ou
/// directement (à la discrétion du store). Le trait n'expose pas la
/// notion de rôle pour rester général.
#[async_trait]
pub trait PermissionStore: Send + Sync {
    /// Retourne l'ensemble des permissions de l'utilisateur. `Ok(empty)`
    /// si l'utilisateur n'a aucune permission (pas une erreur).
    async fn permissions_for(&self, user_id: &str) -> Result<HashSet<String>>;
}

/// Impl in-process, thread-safe (`Mutex<HashMap>`).
///
/// Stocke directement `user_id → permissions` (les rôles sont résolus à
/// l'attribution via [`Self::assign_role`]). Suffisant pour tests/dev ;
/// l'impl PG (jointure `users × roles × permissions`) viendra plus tard.
#[derive(Debug, Default)]
pub struct InMemoryPermissionStore {
    inner: Mutex<HashMap<String, HashSet<String>>>,
}

impl InMemoryPermissionStore {
    /// Crée un store vide.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attribue toutes les permissions d'un `Role` à `user_id`. Idempotent.
    pub fn assign_role(&self, user_id: &str, role: &Role) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("rbac lock empoisonné".into()))?;
        let perms = map.entry(user_id.to_string()).or_default();
        for p in &role.permissions {
            perms.insert(p.clone());
        }
        Ok(())
    }

    /// Attribue une permission individuelle. Idempotent.
    pub fn grant(&self, user_id: &str, permission: impl Into<String>) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("rbac lock empoisonné".into()))?;
        map.entry(user_id.to_string())
            .or_default()
            .insert(permission.into());
        Ok(())
    }

    /// Remplace **toutes** les permissions de `user_id` par `permissions`.
    pub fn replace(&self, user_id: &str, permissions: &HashSet<String>) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("rbac lock empoisonné".into()))?;
        map.insert(user_id.to_string(), permissions.clone());
        Ok(())
    }

    /// Retire une permission. No-op si l'utilisateur ne l'avait pas.
    pub fn revoke(&self, user_id: &str, permission: &str) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("rbac lock empoisonné".into()))?;
        if let Some(set) = map.get_mut(user_id) {
            set.remove(permission);
        }
        Ok(())
    }
}

#[async_trait]
impl PermissionStore for InMemoryPermissionStore {
    async fn permissions_for(&self, user_id: &str) -> Result<HashSet<String>> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("rbac lock empoisonné".into()))?;
        Ok(map.get(user_id).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ───── Permission ─────

    #[test]
    fn permission_constructs_from_string() {
        let p = Permission::new("workflows.read").unwrap();
        assert_eq!(p.as_str(), "workflows.read");
    }

    #[test]
    fn permission_rejects_empty() {
        let err = Permission::new("").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("vide")));
    }

    #[test]
    fn permission_round_trips_through_json() {
        let p = Permission::from("runs.execute");
        let s = serde_json::to_string(&p).unwrap();
        let back: Permission = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    // ───── has_permission ─────

    #[test]
    fn has_permission_exact_match() {
        let granted: HashSet<String> = ["workflows.read".into(), "runs.read".into()]
            .into_iter()
            .collect();
        assert!(has_permission(&granted, "workflows.read"));
        assert!(has_permission(&granted, "runs.read"));
        assert!(!has_permission(&granted, "workflows.write"));
    }

    #[test]
    fn has_permission_wildcard_covers_everything() {
        let granted: HashSet<String> = ["*".into()].into_iter().collect();
        assert!(has_permission(&granted, "workflows.write"));
        assert!(has_permission(&granted, "users.admin"));
        assert!(has_permission(&granted, "anything.at.all"));
    }

    #[test]
    fn has_permission_empty_set_grants_nothing() {
        let empty: HashSet<String> = HashSet::new();
        assert!(!has_permission(&empty, "workflows.read"));
    }

    // ───── Role ─────

    #[test]
    fn role_builders_compose_expected_permissions() {
        let editor = Role::editor();
        assert!(editor.permissions.contains(PERM_WORKFLOWS_WRITE));
        assert!(editor.permissions.contains(PERM_WORKFLOWS_READ));
        assert!(!editor.permissions.contains(PERM_USERS_ADMIN));
        let admin = Role::super_admin();
        assert!(admin.permissions.contains(PERM_WILDCARD));
        let viewer = Role::viewer();
        assert!(viewer.permissions.contains(PERM_WORKFLOWS_READ));
        assert!(!viewer.permissions.contains(PERM_WORKFLOWS_WRITE));
    }

    // ───── InMemoryPermissionStore ─────

    #[tokio::test]
    async fn empty_store_returns_empty_set() {
        let s = InMemoryPermissionStore::new();
        let perms = s.permissions_for("user-1").await.unwrap();
        assert!(perms.is_empty());
    }

    #[tokio::test]
    async fn assign_role_grants_all_role_permissions() {
        let s = InMemoryPermissionStore::new();
        s.assign_role("user-1", &Role::editor()).unwrap();
        let perms = s.permissions_for("user-1").await.unwrap();
        assert!(perms.contains(PERM_WORKFLOWS_WRITE));
        assert!(perms.contains(PERM_RUNS_EXECUTE));
        assert!(!perms.contains(PERM_USERS_ADMIN));
    }

    #[tokio::test]
    async fn assign_role_is_idempotent() {
        let s = InMemoryPermissionStore::new();
        s.assign_role("u", &Role::viewer()).unwrap();
        s.assign_role("u", &Role::viewer()).unwrap();
        let perms = s.permissions_for("u").await.unwrap();
        assert_eq!(perms.len(), 2); // viewer = 2 perms, pas dupliquées
    }

    #[tokio::test]
    async fn assign_multiple_roles_accumulates() {
        let s = InMemoryPermissionStore::new();
        s.assign_role("u", &Role::viewer()).unwrap();
        s.assign_role("u", &Role::editor()).unwrap();
        let perms = s.permissions_for("u").await.unwrap();
        // editor ⊇ viewer ; on doit avoir au moins les permissions de editor
        assert!(perms.contains(PERM_WORKFLOWS_WRITE));
        assert!(perms.contains(PERM_RUNS_READ));
    }

    #[tokio::test]
    async fn grant_and_revoke_work_individually() {
        let s = InMemoryPermissionStore::new();
        s.grant("u", PERM_USERS_ADMIN).unwrap();
        assert!(s
            .permissions_for("u")
            .await
            .unwrap()
            .contains(PERM_USERS_ADMIN));
        s.revoke("u", PERM_USERS_ADMIN).unwrap();
        assert!(!s
            .permissions_for("u")
            .await
            .unwrap()
            .contains(PERM_USERS_ADMIN));
    }

    #[tokio::test]
    async fn revoke_unknown_user_is_noop() {
        let s = InMemoryPermissionStore::new();
        s.revoke("unknown", "x").unwrap();
        assert!(s.permissions_for("unknown").await.unwrap().is_empty());
    }

    #[test]
    fn standard_roles_are_resolved_by_name() {
        assert_eq!(Role::standard("admin"), Some(Role::super_admin()));
        assert_eq!(Role::standard("editor"), Some(Role::editor()));
        assert_eq!(Role::standard("viewer"), Some(Role::viewer()));
        assert_eq!(Role::standard("root"), None);
        assert_eq!(Role::standard("custom"), None);
    }

    #[test]
    fn role_of_maps_permissions_back_to_a_role() {
        assert_eq!(role_of(&Role::super_admin().permissions), ROLE_ADMIN);
        assert_eq!(role_of(&Role::editor().permissions), ROLE_EDITOR);
        assert_eq!(role_of(&Role::viewer().permissions), ROLE_VIEWER);
        // Le wildcard l'emporte sur tout le reste.
        let mut admin_plus = Role::viewer().permissions;
        admin_plus.insert(PERM_WILDCARD.into());
        assert_eq!(role_of(&admin_plus), ROLE_ADMIN);
        // Rôle standard augmenté d'une permission : personnalisé.
        let mut editor_plus = Role::editor().permissions;
        editor_plus.insert(PERM_USERS_ADMIN.into());
        assert_eq!(role_of(&editor_plus), ROLE_CUSTOM);
        assert_eq!(role_of(&HashSet::new()), ROLE_CUSTOM);
    }

    #[tokio::test]
    async fn replace_overwrites_all_permissions() {
        let s = InMemoryPermissionStore::new();
        s.assign_role("u", &Role::editor()).unwrap();
        s.replace("u", &Role::viewer().permissions).unwrap();
        assert_eq!(
            s.permissions_for("u").await.unwrap(),
            Role::viewer().permissions
        );
    }

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryPermissionStore>();
    }
}

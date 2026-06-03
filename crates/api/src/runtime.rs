//! Sélection du backend de persistance au démarrage (dette #107, constat 2).
//!
//! Le mode est piloté par la variable d'environnement `AUTOME_PERSISTENCE` :
//! - `memory` (défaut) : stores in-process (dev, tests, single-node) ;
//! - `postgres` : stores adossés à PostgreSQL (`autome-persistence`), partagés
//!   entre répliques et persistés.
//!
//! En `postgres`, l'[`AuthState`] est bâti sur `PgRevocationStore` /
//! `PgPermissionStore` / `PgAuditSink` (pool unique partagé). Le `TokenService`
//! vient toujours de `config.jwt_secret` (constat 1).

use std::sync::Arc;

use autome_core::{Config, Error, Result};
use autome_persistence::{connect, PgAuditSink, PgPermissionStore, PgRevocationStore};

use crate::auth::{token_service_from_config, AuthState};

/// Backend de persistance retenu pour cette exécution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceMode {
    /// Stores in-process (défaut).
    Memory,
    /// Stores PostgreSQL (`autome-persistence`).
    Postgres,
}

impl PersistenceMode {
    /// Lit le mode depuis `AUTOME_PERSISTENCE` (défaut : `Memory`).
    pub fn from_env() -> Result<Self> {
        Self::parse(std::env::var("AUTOME_PERSISTENCE").ok().as_deref())
    }

    /// Parse une valeur optionnelle (`None`/`""` ⇒ `Memory`).
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "" | "memory" | "in-memory" => Ok(Self::Memory),
            "postgres" | "postgresql" | "pg" => Ok(Self::Postgres),
            other => Err(Error::Invalid(format!(
                "AUTOME_PERSISTENCE invalide : `{other}` (attendu : memory | postgres)"
            ))),
        }
    }
}

/// Construit l'[`AuthState`] selon le `mode` :
/// - `Memory` : [`AuthState::from_config`] (in-memory) ;
/// - `Postgres` : pool partagé + stores PG. Le pool est créé sans tester la
///   connexion (`build_unchecked`) — les erreurs DB remontent au premier usage.
pub fn build_auth_state(config: &Config, mode: PersistenceMode) -> Result<AuthState> {
    match mode {
        PersistenceMode::Memory => Ok(AuthState::from_config(config)),
        PersistenceMode::Postgres => {
            let pool = connect(&config.database_url)?;
            Ok(AuthState::new(
                token_service_from_config(config),
                Arc::new(PgRevocationStore::new(pool.clone())),
                Arc::new(PgPermissionStore::new(pool.clone())),
                // Audit persistant en base ; passer à NoopAuditSink si l'on
                // veut explicitement désactiver l'audit en prod.
                Arc::new(PgAuditSink::new(pool)),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_to_memory() {
        assert_eq!(
            PersistenceMode::parse(None).unwrap(),
            PersistenceMode::Memory
        );
        assert_eq!(
            PersistenceMode::parse(Some("")).unwrap(),
            PersistenceMode::Memory
        );
        assert_eq!(
            PersistenceMode::parse(Some("memory")).unwrap(),
            PersistenceMode::Memory
        );
    }

    #[test]
    fn parse_postgres_variants() {
        for v in ["postgres", "Postgres", "POSTGRESQL", " pg "] {
            assert_eq!(
                PersistenceMode::parse(Some(v)).unwrap(),
                PersistenceMode::Postgres,
                "valeur: {v}"
            );
        }
    }

    #[test]
    fn parse_rejects_unknown() {
        let err = PersistenceMode::parse(Some("redis")).unwrap_err();
        assert!(matches!(err, Error::Invalid(m) if m.contains("AUTOME_PERSISTENCE")));
    }

    #[test]
    fn memory_mode_builds_in_memory_auth_state() {
        let config = Config::load(|_| None).expect("config dev");
        // Ne doit pas toucher au réseau ni échouer.
        let _state = build_auth_state(&config, PersistenceMode::Memory).unwrap();
    }

    #[test]
    fn postgres_mode_builds_without_connecting() {
        // `build_unchecked` ⇒ la construction n'ouvre pas de connexion ; on peut
        // bâtir l'état avec un DSN qui ne répond pas (les erreurs viendraient au
        // premier usage, pas ici).
        let config = Config::load(|k| match k {
            "APP_PROFILE" => Some("prod".into()),
            "DATABASE_URL" => Some("postgres://u:p@127.0.0.1:1/none".into()),
            "JWT_SECRET" => Some("prod-secret-32-characters-minimum!!".into()),
            "ENCRYPTION_KEY" => Some("prod-encryption-key".into()),
            _ => None,
        })
        .expect("config prod");
        let _state = build_auth_state(&config, PersistenceMode::Postgres).unwrap();
    }
}

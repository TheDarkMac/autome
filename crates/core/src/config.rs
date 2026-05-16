//! Configuration centralisée du backend (F1.6).
//!
//! Source unique de vérité : variables d'environnement, avec des défauts
//! par **profil** (`dev`/`prod`). Les secrets ne sont jamais imprimés
//! (impl `Debug` redactée — ARCHITECTURE.md §8 « secrets jamais en clair »).
//! En profil `prod`, les secrets sont **obligatoires et non-défaut** : la
//! validation échoue au démarrage sinon (fail-fast).
//!
//! **Hors périmètre F1.6** : le chiffrement effectif des secrets relève de
//! F6.3, l'usage du secret JWT de F6.1. Ici on ne fait que charger/valider.

use std::fmt;

use crate::{Error, Result};

/// Profil d'exécution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Développement local : défauts permissifs, secrets facultatifs.
    Dev,
    /// Production : secrets obligatoires, écoute sur toutes les interfaces.
    Prod,
}

impl Profile {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "dev" | "development" | "" => Ok(Self::Dev),
            "prod" | "production" => Ok(Self::Prod),
            other => Err(Error::Invalid(format!(
                "APP_PROFILE invalide : `{other}` (attendu : dev | prod)"
            ))),
        }
    }
}

/// Valeur secrète : déréférencable mais jamais affichée en clair.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Accès explicite à la valeur en clair (à n'utiliser qu'au point
    /// d'usage : signature JWT, dérivation de clé…).
    pub fn expose(&self) -> &str {
        &self.0
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redaction : ni la valeur ni sa longueur ne fuitent.
        f.write_str("Secret(***)")
    }
}

/// Configuration applicative chargée et validée.
#[derive(Debug, Clone)]
pub struct Config {
    /// Profil d'exécution.
    pub profile: Profile,
    /// Adresse d'écoute HTTP de l'API.
    pub http_addr: String,
    /// URL de connexion PostgreSQL.
    pub database_url: String,
    /// URL de la file de messages (vide tant que F5.1 n'est pas branchée).
    pub queue_url: String,
    /// Secret de signature des JWT (usage : F6.1).
    pub jwt_secret: Secret,
    /// Clé de chiffrement des credentials au repos (usage : F6.3).
    pub encryption_key: Secret,
}

/// Valeurs par défaut de DÉVELOPPEMENT — jamais utilisables en prod
/// (la validation prod les rejette explicitement).
const DEV_DATABASE_URL: &str = "postgres://postgres:postgres@localhost:5432/autome";
const DEV_JWT_SECRET: &str = "dev-insecure-jwt-secret";
const DEV_ENCRYPTION_KEY: &str = "dev-insecure-encryption-key";

impl Config {
    /// Charge la configuration depuis l'environnement du processus.
    pub fn from_env() -> Result<Self> {
        Self::load(|k| std::env::var(k).ok())
    }

    /// Charge la configuration depuis une source de clés arbitraire.
    ///
    /// Injectable → testable sans toucher à l'environnement global.
    pub fn load(src: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str, default: &str| {
            src(key)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };

        let profile = Profile::parse(&src("APP_PROFILE").unwrap_or_default())?;

        let default_addr = match profile {
            Profile::Dev => "127.0.0.1:8080",
            Profile::Prod => "0.0.0.0:8080",
        };

        let config = Self {
            profile,
            http_addr: get("APP_HTTP_ADDR", default_addr),
            database_url: get("DATABASE_URL", DEV_DATABASE_URL),
            queue_url: get("QUEUE_URL", ""),
            jwt_secret: Secret(get("JWT_SECRET", DEV_JWT_SECRET)),
            encryption_key: Secret(get("ENCRYPTION_KEY", DEV_ENCRYPTION_KEY)),
        };
        config.validate()?;
        Ok(config)
    }

    /// Invariants. En `prod`, refuse les secrets vides ou laissés aux
    /// valeurs de dev (fail-fast au démarrage).
    fn validate(&self) -> Result<()> {
        if self.http_addr.is_empty() {
            return Err(Error::Invalid("APP_HTTP_ADDR vide".into()));
        }
        if self.profile == Profile::Prod {
            let insecure = [
                ("JWT_SECRET", &self.jwt_secret, DEV_JWT_SECRET),
                ("ENCRYPTION_KEY", &self.encryption_key, DEV_ENCRYPTION_KEY),
            ];
            for (name, secret, dev_default) in insecure {
                if secret.is_empty() || secret.expose() == dev_default {
                    return Err(Error::Invalid(format!(
                        "{name} doit être défini (non-défaut) en profil prod"
                    )));
                }
            }
            if self.database_url == DEV_DATABASE_URL {
                return Err(Error::Invalid(
                    "DATABASE_URL doit être défini en profil prod".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn src(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn dev_profile_uses_defaults() {
        let c = Config::load(src(&[])).expect("dev defaults");
        assert_eq!(c.profile, Profile::Dev);
        assert_eq!(c.http_addr, "127.0.0.1:8080");
        assert_eq!(c.database_url, DEV_DATABASE_URL);
        assert_eq!(c.queue_url, "");
    }

    #[test]
    fn prod_rejects_default_secrets() {
        let err = Config::load(src(&[("APP_PROFILE", "prod")])).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[test]
    fn prod_ok_when_all_secrets_provided() {
        let c = Config::load(src(&[
            ("APP_PROFILE", "production"),
            ("DATABASE_URL", "postgres://u:p@db:5432/prod"),
            ("JWT_SECRET", "real-secret"),
            ("ENCRYPTION_KEY", "real-key"),
        ]))
        .expect("prod valid");
        assert_eq!(c.profile, Profile::Prod);
        assert_eq!(c.http_addr, "0.0.0.0:8080");
    }

    #[test]
    fn invalid_profile_is_rejected() {
        let err = Config::load(src(&[("APP_PROFILE", "staging")])).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[test]
    fn secret_debug_is_redacted() {
        let c = Config::load(src(&[])).expect("dev");
        let dump = format!("{c:?}");
        assert!(dump.contains("Secret(***)"));
        assert!(!dump.contains(DEV_JWT_SECRET));
        assert!(!dump.contains(DEV_ENCRYPTION_KEY));
        // L'accès explicite reste possible au point d'usage.
        assert_eq!(c.jwt_secret.expose(), DEV_JWT_SECRET);
    }

    #[test]
    fn empty_env_values_fall_back_to_defaults() {
        let c = Config::load(src(&[("APP_HTTP_ADDR", "")])).expect("dev");
        assert_eq!(c.http_addr, "127.0.0.1:8080");
    }
}

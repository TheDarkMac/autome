//! Attente du schéma au démarrage (#271).
//!
//! Le worker ne migre pas : l'API applique les migrations au démarrage
//! (`AUTOME_MIGRATE`). Déployés ensemble, le nouveau worker peut démarrer
//! avant que l'API ait migré et exécuter des runs sur l'ancien schéma. En
//! `postgres`, il attend donc avant de consommer que le journal
//! `schema_migrations` contienne toutes ses migrations
//! ([`autome_persistence::wait_for_schema_ready`]), au plus
//! [`DEFAULT_SCHEMA_WAIT`] (`AUTOME_WORKER_SCHEMA_WAIT_SECS`), puis refuse de
//! démarrer en nommant les migrations manquantes.

use std::time::Duration;

/// Attente par défaut : le temps qu'une API démarre et migre, sous la
/// `startupProbe` de `k8s/worker.yaml` (avec l'attente du broker).
pub const DEFAULT_SCHEMA_WAIT: Duration = Duration::from_secs(120);

/// `AUTOME_WORKER_SCHEMA_WAIT_SECS` (secondes, défaut 120, `0` = une seule
/// vérification).
pub fn schema_wait_from_env() -> Result<Duration, String> {
    schema_wait_from(std::env::var("AUTOME_WORKER_SCHEMA_WAIT_SECS").ok())
}

fn schema_wait_from(value: Option<String>) -> Result<Duration, String> {
    match value {
        None => Ok(DEFAULT_SCHEMA_WAIT),
        Some(v) if v.trim().is_empty() => Ok(DEFAULT_SCHEMA_WAIT),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| {
                format!("AUTOME_WORKER_SCHEMA_WAIT_SECS : nombre de secondes attendu, obtenu `{v}`")
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_configuration() {
        assert_eq!(schema_wait_from(None).unwrap(), DEFAULT_SCHEMA_WAIT);
        assert_eq!(
            schema_wait_from(Some(" ".into())).unwrap(),
            DEFAULT_SCHEMA_WAIT
        );
        assert_eq!(
            schema_wait_from(Some("300".into())).unwrap(),
            Duration::from_secs(300)
        );
        assert_eq!(schema_wait_from(Some("0".into())).unwrap(), Duration::ZERO);
        let err = schema_wait_from(Some("2m".into())).unwrap_err();
        assert!(err.contains("AUTOME_WORKER_SCHEMA_WAIT_SECS"), "{err}");
    }
}

//! Runner de migrations embarqué **async** (dette #107).
//!
//! Les fichiers `migrations/*.sql` sont **embarqués** (`include_str!`) : pas
//! besoin de les copier dans l'image. [`run_migrations`] applique celles qui
//! manquent, dans l'ordre, en suivant un journal `schema_migrations`.
//!
//! Garanties : **idempotent** (ré-exécuter n'applique rien de plus),
//! **atomique** (transaction unique — les DDL PostgreSQL sont transactionnelles
//! — échec ⇒ rollback), **concurrent-safe** (`pg_advisory_xact_lock` sérialise
//! les répliques qui démarreraient ensemble).
//!
//! Hypothèse : base **gérée par ce runner** (schéma vierge au départ).
//!
//! Seule l'API migre (`AUTOME_MIGRATE`). Le worker, lui, attend avec
//! [`wait_for_schema`] que le journal contienne toutes ses migrations avant de
//! consommer un run (#271) : déployé en même temps qu'une API qui migre, il
//! exécuterait sinon ses runs sur l'ancien schéma.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use autome_core::{Error, Result};

use crate::pool::PgPool;

/// Clé arbitraire (mais stable) du verrou applicatif de migration.
const MIGRATION_LOCK_KEY: i64 = 0x4155_546F_6D65; // "AUTOme"

/// Migrations embarquées, **dans l'ordre d'application**. Ajouter toute
/// nouvelle migration ici (le nom sert de clé d'idempotence).
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_users.sql",
        include_str!("../../../migrations/0001_users.sql"),
    ),
    (
        "0002_workflows.sql",
        include_str!("../../../migrations/0002_workflows.sql"),
    ),
    (
        "0003_runs.sql",
        include_str!("../../../migrations/0003_runs.sql"),
    ),
    (
        "0004_credentials.sql",
        include_str!("../../../migrations/0004_credentials.sql"),
    ),
    (
        "0005_indexes_constraints.sql",
        include_str!("../../../migrations/0005_indexes_constraints.sql"),
    ),
    (
        "0006_auth_revocations.sql",
        include_str!("../../../migrations/0006_auth_revocations.sql"),
    ),
    (
        "0007_rbac_permissions.sql",
        include_str!("../../../migrations/0007_rbac_permissions.sql"),
    ),
    (
        "0008_audit_events.sql",
        include_str!("../../../migrations/0008_audit_events.sql"),
    ),
    (
        "0009_align_ids_text.sql",
        include_str!("../../../migrations/0009_align_ids_text.sql"),
    ),
    (
        "0010_resource_owners.sql",
        include_str!("../../../migrations/0010_resource_owners.sql"),
    ),
    (
        "0011_connector_tokens.sql",
        include_str!("../../../migrations/0011_connector_tokens.sql"),
    ),
    (
        "0012_login_attempts.sql",
        include_str!("../../../migrations/0012_login_attempts.sql"),
    ),
    (
        "0013_session_versions.sql",
        include_str!("../../../migrations/0013_session_versions.sql"),
    ),
    (
        "0014_workflow_namespaces.sql",
        include_str!("../../../migrations/0014_workflow_namespaces.sql"),
    ),
    (
        "0015_throttle_keys.sql",
        include_str!("../../../migrations/0015_throttle_keys.sql"),
    ),
    (
        "0016_webhook_endpoints.sql",
        include_str!("../../../migrations/0016_webhook_endpoints.sql"),
    ),
    (
        "0017_user_admin.sql",
        include_str!("../../../migrations/0017_user_admin.sql"),
    ),
    (
        "0018_connector_token_invalidation.sql",
        include_str!("../../../migrations/0018_connector_token_invalidation.sql"),
    ),
    (
        "0019_trigger_schedules.sql",
        include_str!("../../../migrations/0019_trigger_schedules.sql"),
    ),
    (
        "0020_email_verification.sql",
        include_str!("../../../migrations/0020_email_verification.sql"),
    ),
    (
        "0021_run_outbox_resume.sql",
        include_str!("../../../migrations/0021_run_outbox_resume.sql"),
    ),
    (
        "0022_user_mfa.sql",
        include_str!("../../../migrations/0022_user_mfa.sql"),
    ),
    (
        "0023_drop_credentials.sql",
        include_str!("../../../migrations/0023_drop_credentials.sql"),
    ),
    (
        "0024_connector_token_reconnect_notice.sql",
        include_str!("../../../migrations/0024_connector_token_reconnect_notice.sql"),
    ),
    (
        "0025_legacy_runs_namespace.sql",
        include_str!("../../../migrations/0025_legacy_runs_namespace.sql"),
    ),
];

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Applique les migrations manquantes. Retourne le nombre de migrations
/// effectivement appliquées lors de cet appel (0 si la base est à jour).
pub async fn run_migrations(pool: &PgPool) -> Result<usize> {
    let mut client = pool.get().await.map_err(|e| storage_err("pool", e))?;
    let tx = client
        .transaction()
        .await
        .map_err(|e| storage_err("begin", e))?;

    // Sérialise les exécutions concurrentes (relâché en fin de transaction).
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK_KEY])
        .await
        .map_err(|e| storage_err("lock", e))?;

    tx.batch_execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (\
            name TEXT PRIMARY KEY, \
            applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .await
    .map_err(|e| storage_err("schema_migrations", e))?;

    let applied: HashSet<String> = tx
        .query("SELECT name FROM schema_migrations", &[])
        .await
        .map_err(|e| storage_err("read journal", e))?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();

    let mut count = 0usize;
    for (name, sql) in MIGRATIONS {
        if applied.contains(*name) {
            continue;
        }
        tx.batch_execute(sql)
            .await
            .map_err(|e| storage_err(&format!("apply {name}"), e))?;
        tx.execute("INSERT INTO schema_migrations (name) VALUES ($1)", &[name])
            .await
            .map_err(|e| storage_err(&format!("journal {name}"), e))?;
        count += 1;
    }

    tx.commit().await.map_err(|e| storage_err("commit", e))?;
    Ok(count)
}

/// Migrations embarquées absentes du journal de la base, dans l'ordre de
/// [`MIGRATIONS`] : toutes si le journal n'existe pas encore (base vierge).
/// Lecture seule : ne crée pas le journal, ne prend pas le verrou de
/// migration. Une entrée du journal inconnue de ce binaire (base migrée par
/// une version plus récente) est ignorée.
pub async fn pending_migrations(pool: &PgPool) -> Result<Vec<&'static str>> {
    let client = pool.get().await.map_err(|e| storage_err("pool", e))?;
    let journal: bool = client
        .query_one("SELECT to_regclass('schema_migrations') IS NOT NULL", &[])
        .await
        .map_err(|e| storage_err("journal lookup", e))?
        .get(0);
    let applied: HashSet<String> = if journal {
        client
            .query("SELECT name FROM schema_migrations", &[])
            .await
            .map_err(|e| storage_err("read journal", e))?
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect()
    } else {
        HashSet::new()
    };
    Ok(MIGRATIONS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !applied.contains(*name))
        .collect())
}

/// Premier intervalle entre deux vérifications de [`wait_for_schema`].
const SCHEMA_POLL_FIRST: Duration = Duration::from_millis(500);
/// Plafond de l'intervalle (doublé à chaque vérification).
const SCHEMA_POLL_MAX: Duration = Duration::from_secs(5);

/// Attend que toutes les [`MIGRATIONS`] figurent au journal, au plus
/// `timeout` (`0` = une seule vérification). Base injoignable et migrations
/// manquantes sont revérifiées de la même façon, avec un intervalle de
/// 0,5 s doublé jusqu'à 5 s. Au-delà du délai : erreur listant les migrations
/// manquantes, ou la dernière erreur d'accès à la base.
pub async fn wait_for_schema(pool: &PgPool, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut delay = SCHEMA_POLL_FIRST;
    loop {
        let outcome = match pending_migrations(pool).await {
            Ok(pending) if pending.is_empty() => return Ok(()),
            Ok(pending) => Error::Invalid(format!(
                "schéma PostgreSQL en retard : {} migration(s) manquante(s) ({}) — \
                 l'API les applique au démarrage (AUTOME_MIGRATE=true), \
                 la déployer avant le worker",
                pending.len(),
                pending.join(", ")
            )),
            Err(e) => e,
        };
        if Instant::now() + delay >= deadline {
            return Err(outcome);
        }
        tracing::warn!(
            retry_in_ms = delay.as_millis() as u64,
            error = %outcome,
            "schéma pas encore prêt, nouvelle vérification"
        );
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(SCHEMA_POLL_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_ordered_and_unique() {
        let names: Vec<&str> = MIGRATIONS.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "les migrations doivent être triées par nom");

        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "noms de migration dupliqués");

        assert!(MIGRATIONS.iter().all(|(_, sql)| !sql.trim().is_empty()));
    }
}

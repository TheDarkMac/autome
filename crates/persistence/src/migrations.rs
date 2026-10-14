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

use std::collections::HashSet;

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

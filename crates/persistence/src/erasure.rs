//! [`AccountEraser`] adossé à PostgreSQL : effacement d'un compte en **une
//! transaction** (RGPD, #229).
//!
//! Le compte est verrouillé (`SELECT … FOR UPDATE`) puis, dans l'ordre :
//! runs du propriétaire (`run_steps` en cascade), workflows de son espace
//! (`workflow_versions`, `webhook_endpoints`, `trigger_schedules` en
//! cascade), tokens de connecteurs, permissions, pseudonymisation de
//! l'audit, puis le compte lui-même. Une erreur à n'importe quelle étape
//! annule tout.
//!
//! `runs.owner_id`, `connector_tokens.user_id` et `user_permissions.user_id`
//! sont des `TEXT` sans clé étrangère vers `users` (identité = `claims.sub`,
//! cf. migrations 0007, 0010, 0011) : aucune cascade ne les atteint depuis
//! `users`, d'où les suppressions explicites. Les runs du propriétaire et la
//! colonne `resource` de l'audit ne sont pas indexés pour cette recherche :
//! l'effacement, rare, parcourt ces tables.

use async_trait::async_trait;
use autome_core::{AccountEraser, ErasureReport, Error};

use crate::pool::PgPool;
use crate::users::is_uuid;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

fn count(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Effacement de comptes dans PostgreSQL.
#[derive(Clone)]
pub struct PgAccountEraser {
    pool: PgPool,
}

impl PgAccountEraser {
    /// Construit l'effaceur sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AccountEraser for PgAccountEraser {
    async fn erase(
        &self,
        user_id: &str,
        pseudonym: &str,
    ) -> autome_core::Result<Option<ErasureReport>> {
        if !is_uuid(user_id) {
            return Ok(None);
        }
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // Verrou du compte : deux effacements concurrents ne se mélangent pas
        // (le second attend, puis ne trouve plus rien).
        let Some(row) = tx
            .query_opt(
                "SELECT email FROM users WHERE id = $1::text::uuid FOR UPDATE",
                &[&user_id],
            )
            .await
            .map_err(|e| storage_err("lock user", e))?
        else {
            return Ok(None);
        };
        let email: String = row.get(0);

        let runs = tx
            .execute("DELETE FROM runs WHERE owner_id = $1", &[&user_id])
            .await
            .map_err(|e| storage_err("erase runs", e))?;
        let workflows = tx
            .execute("DELETE FROM workflows WHERE owner_id = $1", &[&user_id])
            .await
            .map_err(|e| storage_err("erase workflows", e))?;
        let connector_tokens = tx
            .execute(
                "DELETE FROM connector_tokens WHERE user_id = $1",
                &[&user_id],
            )
            .await
            .map_err(|e| storage_err("erase connector tokens", e))?;
        tx.execute(
            "DELETE FROM user_permissions WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .map_err(|e| storage_err("erase permissions", e))?;

        // L'acteur vaut l'id (actions authentifiées) ou l'email (échecs de
        // connexion, demandes de réinitialisation) ; la ressource vaut l'id
        // pour les actions d'administration qui visent le compte.
        let identifiers = [user_id, email.as_str()];
        let audit_events = tx
            .execute(
                "UPDATE audit_events SET \
                   actor = CASE WHEN actor = ANY($1) THEN $2 ELSE actor END, \
                   resource = CASE WHEN resource = ANY($1) THEN $2 ELSE resource END \
                 WHERE actor = ANY($1) OR resource = ANY($1)",
                &[&&identifiers[..], &pseudonym],
            )
            .await
            .map_err(|e| storage_err("pseudonymize audit", e))?;

        tx.execute("DELETE FROM users WHERE id = $1::text::uuid", &[&user_id])
            .await
            .map_err(|e| storage_err("erase user", e))?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;

        Ok(Some(ErasureReport {
            workflows: count(workflows),
            runs: count(runs),
            connector_tokens: count(connector_tokens),
            audit_events: count(audit_events),
        }))
    }
}

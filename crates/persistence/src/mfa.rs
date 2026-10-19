//! [`MfaStore`] adossé à PostgreSQL (tables `user_mfa` + `mfa_recovery_codes`,
//! migration `0022_user_mfa.sql`, #245).
//!
//! Toutes les écritures qui doivent résister à la concurrence sont des
//! **écritures conditionnelles** sur une seule ligne : PostgreSQL réévalue la
//! condition après le verrou de ligne, si bien que de deux requêtes qui
//! présentent le même code TOTP (ou le même code de secours), une seule voit
//! sa ligne modifiée. Les lignes disparaissent avec le compte (clés étrangères
//! en cascade).

use std::collections::HashSet;

use async_trait::async_trait;
use autome_core::{Error, MfaRecord, MfaStore};

use crate::pool::PgPool;
use crate::users::is_uuid;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Second facteur persisté dans PostgreSQL.
#[derive(Clone)]
pub struct PgMfaStore {
    pool: PgPool,
}

impl PgMfaStore {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MfaStore for PgMfaStore {
    async fn get(&self, user_id: &str) -> autome_core::Result<Option<MfaRecord>> {
        if !is_uuid(user_id) {
            return Ok(None);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                "SELECT m.secret, m.enabled_at IS NOT NULL, m.last_step, \
                   (SELECT count(*) FROM mfa_recovery_codes c WHERE c.user_id = m.user_id) \
                 FROM user_mfa m WHERE m.user_id = $1::text::uuid",
                &[&user_id],
            )
            .await
            .map_err(|e| storage_err("get mfa", e))?;
        Ok(row.map(|r| MfaRecord {
            secret: r.get(0),
            enabled: r.get(1),
            last_step: r.get(2),
            recovery_codes_left: usize::try_from(r.get::<_, i64>(3)).unwrap_or(0),
        }))
    }

    async fn begin_enrollment(&self, user_id: &str, secret: &str) -> autome_core::Result<bool> {
        if !is_uuid(user_id) {
            return Ok(false);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Compte inconnu : le `SELECT` ne rend aucune ligne, rien n'est
        // inséré. MFA active : le `WHERE` de la mise à jour l'exclut.
        let n = client
            .execute(
                "INSERT INTO user_mfa (user_id, secret) \
                 SELECT id, $2 FROM users WHERE id = $1::text::uuid \
                 ON CONFLICT (user_id) DO UPDATE \
                   SET secret = EXCLUDED.secret, last_step = NULL, created_at = now() \
                   WHERE user_mfa.enabled_at IS NULL",
                &[&user_id, &secret],
            )
            .await
            .map_err(|e| storage_err("begin mfa enrollment", e))?;
        Ok(n == 1)
    }

    async fn enable(
        &self,
        user_id: &str,
        secret: &str,
        step: i64,
        recovery_hashes: &[String],
    ) -> autome_core::Result<bool> {
        if !is_uuid(user_id) {
            return Ok(false);
        }
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;
        let n = tx
            .execute(
                "UPDATE user_mfa SET enabled_at = now(), last_step = $3 \
                 WHERE user_id = $1::text::uuid AND enabled_at IS NULL AND secret = $2",
                &[&user_id, &secret, &step],
            )
            .await
            .map_err(|e| storage_err("enable mfa", e))?;
        if n != 1 {
            return Ok(false);
        }
        replace_codes(&tx, user_id, recovery_hashes).await?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(true)
    }

    async fn consume_step(&self, user_id: &str, step: i64) -> autome_core::Result<bool> {
        if !is_uuid(user_id) {
            return Ok(false);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute(
                "UPDATE user_mfa SET last_step = $2 \
                 WHERE user_id = $1::text::uuid AND enabled_at IS NOT NULL \
                   AND (last_step IS NULL OR last_step < $2)",
                &[&user_id, &step],
            )
            .await
            .map_err(|e| storage_err("consume totp step", e))?;
        Ok(n == 1)
    }

    async fn consume_recovery_code(
        &self,
        user_id: &str,
        code_hash: &str,
    ) -> autome_core::Result<bool> {
        if !is_uuid(user_id) {
            return Ok(false);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute(
                "DELETE FROM mfa_recovery_codes c USING user_mfa m \
                 WHERE c.user_id = $1::text::uuid AND c.code_hash = $2 \
                   AND m.user_id = c.user_id AND m.enabled_at IS NOT NULL",
                &[&user_id, &code_hash],
            )
            .await
            .map_err(|e| storage_err("consume recovery code", e))?;
        Ok(n == 1)
    }

    async fn replace_recovery_codes(
        &self,
        user_id: &str,
        recovery_hashes: &[String],
    ) -> autome_core::Result<bool> {
        if !is_uuid(user_id) {
            return Ok(false);
        }
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;
        // Verrou de la ligne MFA : deux remplacements concurrents ne
        // mélangent pas leurs codes, une désactivation concurrente attend.
        let active = tx
            .query_opt(
                "SELECT 1 FROM user_mfa \
                 WHERE user_id = $1::text::uuid AND enabled_at IS NOT NULL FOR UPDATE",
                &[&user_id],
            )
            .await
            .map_err(|e| storage_err("lock mfa", e))?;
        if active.is_none() {
            return Ok(false);
        }
        replace_codes(&tx, user_id, recovery_hashes).await?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(true)
    }

    async fn disable(&self, user_id: &str) -> autome_core::Result<bool> {
        if !is_uuid(user_id) {
            return Ok(false);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Les codes de secours suivent (cascade).
        let n = client
            .execute(
                "DELETE FROM user_mfa WHERE user_id = $1::text::uuid",
                &[&user_id],
            )
            .await
            .map_err(|e| storage_err("disable mfa", e))?;
        Ok(n == 1)
    }

    async fn enabled_among(&self, user_ids: &[String]) -> autome_core::Result<HashSet<String>> {
        let ids: Vec<&str> = user_ids
            .iter()
            .map(String::as_str)
            .filter(|id| is_uuid(id))
            .collect();
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let rows = client
            .query(
                "SELECT user_id::text FROM user_mfa \
                 WHERE user_id = ANY($1::text[]::uuid[]) AND enabled_at IS NOT NULL",
                &[&ids],
            )
            .await
            .map_err(|e| storage_err("mfa enabled among", e))?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }
}

/// Remplace les codes de secours de `user_id` dans la transaction `tx`.
async fn replace_codes(
    tx: &tokio_postgres::Transaction<'_>,
    user_id: &str,
    recovery_hashes: &[String],
) -> autome_core::Result<()> {
    tx.execute(
        "DELETE FROM mfa_recovery_codes WHERE user_id = $1::text::uuid",
        &[&user_id],
    )
    .await
    .map_err(|e| storage_err("clear recovery codes", e))?;
    tx.execute(
        "INSERT INTO mfa_recovery_codes (user_id, code_hash) \
         SELECT $1::text::uuid, h FROM unnest($2::text[]) AS h ON CONFLICT DO NOTHING",
        &[&user_id, &recovery_hashes],
    )
    .await
    .map_err(|e| storage_err("insert recovery codes", e))?;
    Ok(())
}

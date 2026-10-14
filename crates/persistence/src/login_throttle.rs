//! [`LoginThrottleStore`] adossé à PostgreSQL (table `login_attempts`) — #182.
//!
//! Partagé par toutes les répliques de l'API et persistant au redémarrage.
//! La réservation est **une seule instruction** (`INSERT … ON CONFLICT DO
//! UPDATE … RETURNING`) : le verrou de ligne sérialise les tentatives
//! concurrentes sur un même email, quelle que soit la réplique qui les reçoit.
//! Schéma : migration `migrations/0012_login_attempts.sql`.

use async_trait::async_trait;
use autome_core::{Error, LoginAttempt, LoginThrottleStore, LOGIN_WINDOW_SECONDS};
use sha2::{Digest, Sha256};

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Clé de la ligne : SHA-256 (hex) de l'email normalisé.
fn email_key(email: &str) -> String {
    Sha256::digest(email.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Compteur de tentatives de connexion persisté dans PostgreSQL.
#[derive(Clone)]
pub struct PgLoginThrottle {
    pool: PgPool,
}

impl PgLoginThrottle {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl LoginThrottleStore for PgLoginThrottle {
    async fn begin_attempt(&self, email: &str, now: i64) -> autome_core::Result<LoginAttempt> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Fenêtre expirée ⇒ on repart à 1 ; sinon +1 (les refus comptent aussi,
        // sans prolonger la fenêtre).
        let row = client
            .query_one(
                "INSERT INTO login_attempts (email_hash, attempts, window_start) \
                 VALUES ($1, 1, $2) \
                 ON CONFLICT (email_hash) DO UPDATE SET \
                   attempts = CASE WHEN $2 - login_attempts.window_start >= $3 THEN 1 \
                                   ELSE login_attempts.attempts + 1 END, \
                   window_start = CASE WHEN $2 - login_attempts.window_start >= $3 THEN $2 \
                                       ELSE login_attempts.window_start END \
                 RETURNING attempts, window_start",
                &[&email_key(email), &now, &LOGIN_WINDOW_SECONDS],
            )
            .await
            .map_err(|e| storage_err("login_attempts.begin", e))?;
        Ok(LoginAttempt::from_counter(row.get(0), row.get(1), now))
    }

    async fn reset(&self, email: &str) -> autome_core::Result<()> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        client
            .execute(
                "DELETE FROM login_attempts WHERE email_hash = $1",
                &[&email_key(email)],
            )
            .await
            .map_err(|e| storage_err("login_attempts.reset", e))?;
        Ok(())
    }

    async fn gc(&self, now: i64) -> autome_core::Result<usize> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let n = client
            .execute(
                "DELETE FROM login_attempts WHERE window_start <= $1",
                &[&(now - LOGIN_WINDOW_SECONDS)],
            )
            .await
            .map_err(|e| storage_err("login_attempts.gc", e))?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_key_is_a_stable_sha256_hex() {
        let k = email_key("joe@example.io");
        assert_eq!(k.len(), 64);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(k, email_key("joe@example.io"));
        assert_ne!(k, email_key("ann@example.io"));
        assert!(!k.contains("joe"));
    }
}

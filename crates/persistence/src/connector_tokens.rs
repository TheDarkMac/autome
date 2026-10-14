//! [`ConnectorTokenStore`] adossé à PostgreSQL (table `connector_tokens`).
//!
//! Persiste les tokens OAuth des connecteurs **par `(user_id, provider_id)`**,
//! **chiffrés au repos** : la colonne `envelope` ne contient que l'enveloppe
//! `EnvelopeCipher` produite par [`seal_tokens`] (même format que
//! `InMemoryConnectorTokenStore`). L'API y écrit au callback OAuth, le worker
//! y lit le token du propriétaire du run (backlog post-audit).
//! Schéma : migration `migrations/0011_connector_tokens.sql`.
//!
//! ## Pont sync → async
//!
//! Le trait [`ConnectorTokenStore`] est **synchrone** : il est appelé depuis
//! du code bloquant (échange OAuth `reqwest::blocking` côté API, moteur dans
//! `spawn_blocking` côté worker). Chaque méthode exécute donc la requête async
//! sur le runtime tokio **courant** via `block_in_place` + `Handle::block_on`.
//! Contrat d'appel : depuis un thread `spawn_blocking`, ou depuis une tâche
//! d'un runtime **multi-thread**. Hors runtime, l'appel échoue proprement ;
//! depuis une tâche d'un runtime `current_thread`, tokio panique.

use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

use autome_connectors::oauth::{open_tokens, seal_tokens, ConnectorTokenStore, TokenSet};
use autome_core::{EnvelopeCipher, Error, KeyProvider, Result};
use tokio::runtime::Handle;

use crate::pool::PgPool;

/// Conversion uniforme des erreurs deadpool/tokio-postgres en `core::Error`.
fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Exécute `fut` jusqu'au bout depuis un contexte synchrone (cf. module).
fn block_on<T>(fut: impl Future<Output = Result<T>>) -> Result<T> {
    let handle = Handle::try_current()
        .map_err(|e| storage_err("connector_tokens: runtime tokio requis", e))?;
    tokio::task::block_in_place(|| handle.block_on(fut))
}

/// Horodatage Unix (ms) courant.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Tokens de connecteurs persistés dans PostgreSQL, chiffrés via `cipher`.
pub struct PgConnectorTokenStore<K: KeyProvider> {
    pool: PgPool,
    cipher: EnvelopeCipher<K>,
}

impl<K: KeyProvider> PgConnectorTokenStore<K> {
    /// Construit le store sur un pool existant et le cipher des credentials.
    pub fn new(pool: PgPool, cipher: EnvelopeCipher<K>) -> Self {
        Self { pool, cipher }
    }
}

impl<K: KeyProvider> ConnectorTokenStore for PgConnectorTokenStore<K> {
    fn store(&self, user_id: &str, provider_id: &str, tokens: &TokenSet) -> Result<()> {
        // Chiffré **avant** de quitter le process : la base ne voit jamais le clair.
        let envelope = seal_tokens(&self.cipher, tokens)?;
        let updated_at_ms = now_ms();
        block_on(async {
            let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
            client
                .execute(
                    "INSERT INTO connector_tokens (user_id, provider_id, envelope, updated_at_ms) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (user_id, provider_id) DO UPDATE \
                     SET envelope = EXCLUDED.envelope, updated_at_ms = EXCLUDED.updated_at_ms",
                    &[&user_id, &provider_id, &envelope, &updated_at_ms],
                )
                .await
                .map_err(|e| storage_err("connector_tokens.store", e))?;
            Ok(())
        })
    }

    fn load(&self, user_id: &str, provider_id: &str) -> Result<Option<TokenSet>> {
        let envelope: Option<String> = block_on(async {
            let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
            let row = client
                .query_opt(
                    "SELECT envelope FROM connector_tokens \
                     WHERE user_id = $1 AND provider_id = $2",
                    &[&user_id, &provider_id],
                )
                .await
                .map_err(|e| storage_err("connector_tokens.load", e))?;
            Ok(row.map(|r| r.get(0)))
        })?;
        envelope
            .map(|env| open_tokens(&self.cipher, &env))
            .transpose()
    }

    fn delete(&self, user_id: &str, provider_id: &str) -> Result<()> {
        block_on(async {
            let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
            client
                .execute(
                    "DELETE FROM connector_tokens WHERE user_id = $1 AND provider_id = $2",
                    &[&user_id, &provider_id],
                )
                .await
                .map_err(|e| storage_err("connector_tokens.delete", e))?;
            Ok(())
        })
    }

    fn list_by_user(&self, user_id: &str) -> Result<Vec<String>> {
        block_on(async {
            let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
            let rows = client
                .query(
                    "SELECT provider_id FROM connector_tokens \
                     WHERE user_id = $1 ORDER BY provider_id",
                    &[&user_id],
                )
                .await
                .map_err(|e| storage_err("connector_tokens.list_by_user", e))?;
            Ok(rows.iter().map(|r| r.get(0)).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::InMemoryKeyProvider;

    #[test]
    fn calls_outside_a_tokio_runtime_fail_cleanly() {
        // Pool sans connexion (deadpool connecte à la demande) : on vérifie le
        // garde-fou du pont, pas l'accès réseau.
        let pool = crate::connect("postgres://u:p@127.0.0.1:1/none").unwrap();
        let store = PgConnectorTokenStore::new(
            pool,
            EnvelopeCipher::new(InMemoryKeyProvider::with_key("v1", [1u8; 32])),
        );
        let err = store.load("alice", "slack").unwrap_err();
        assert!(err.to_string().contains("runtime tokio requis"), "{err}");
    }
}

//! [`UserStore`] adossé à PostgreSQL (tables `users` + `user_permissions`), **async**.
//!
//! Schéma : migrations `0001_users.sql`, `0007_rbac_permissions.sql` et
//! `0017_user_admin.sql` (suspension, index de pagination). Le compte et ses
//! permissions sont écrits dans **une seule transaction** : un crash entre les
//! deux ne laisse ni compte sans droits ni droits orphelins. Il en va de même
//! du remplacement des permissions par l'administration (#208).

use std::collections::HashSet;

use async_trait::async_trait;
use autome_core::{CreateUser, Error, UserPage, UserRecord, UserStore, UserSummary};
use tokio_postgres::Row;

use crate::pool::PgPool;

fn storage_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("storage: {context}: {e}"))
}

/// Colonnes d'un [`UserRecord`], dans l'ordre lu par [`record`].
const RECORD_COLUMNS: &str = "id::text, email, password_hash, disabled_at IS NOT NULL";

fn record(r: &Row) -> UserRecord {
    UserRecord {
        id: r.get(0),
        email: r.get(1),
        password_hash: r.get(2),
        disabled: r.get(3),
    }
}

/// Colonnes d'un [`UserSummary`], dans l'ordre lu par [`summary`] : les
/// permissions de chaque ligne en sous-requête (clé primaire de
/// `user_permissions`), évaluée pour les seules lignes de la page.
const SUMMARY_COLUMNS: &str = "u.id::text, u.email, u.disabled_at IS NOT NULL, \
     (extract(epoch FROM u.created_at) * 1000)::bigint, \
     ARRAY(SELECT p.permission FROM user_permissions p WHERE p.user_id = u.id::text)";

fn summary(r: &Row) -> UserSummary {
    UserSummary {
        id: r.get(0),
        email: r.get(1),
        disabled: r.get(2),
        created_at_ms: r.get(3),
        permissions: r.get::<_, Vec<String>>(4).into_iter().collect(),
    }
}

/// Motif `LIKE` « contient `query` », métacaractères échappés (`\` par défaut).
fn contains_pattern(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len() + 2);
    escaped.push('%');
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped.push('%');
    escaped
}

/// Comptes utilisateurs persistés dans PostgreSQL.
#[derive(Clone)]
pub struct PgUserStore {
    pool: PgPool,
}

impl PgUserStore {
    /// Construit le store sur un pool existant.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UserStore for PgUserStore {
    async fn create(
        &self,
        email: &str,
        password_hash: &str,
        permissions: &HashSet<String>,
    ) -> autome_core::Result<CreateUser> {
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;

        // `ON CONFLICT DO NOTHING RETURNING` : aucune ligne ⇒ email déjà pris,
        // détecté sans dépendre du code d'erreur 23505.
        let row = tx
            .query_opt(
                "INSERT INTO users (email, password_hash) VALUES ($1, $2) \
                 ON CONFLICT (email) DO NOTHING RETURNING id::text",
                &[&email, &password_hash],
            )
            .await
            .map_err(|e| storage_err("create user", e))?;
        let Some(row) = row else {
            return Ok(CreateUser::EmailTaken);
        };
        let id: String = row.get(0);

        for permission in permissions {
            tx.execute(
                "INSERT INTO user_permissions (user_id, permission) VALUES ($1, $2) \
                 ON CONFLICT (user_id, permission) DO NOTHING",
                &[&id, permission],
            )
            .await
            .map_err(|e| storage_err("grant", e))?;
        }
        tx.commit().await.map_err(|e| storage_err("commit", e))?;

        Ok(CreateUser::Created(UserRecord {
            id,
            email: email.to_string(),
            password_hash: password_hash.to_string(),
            disabled: false,
        }))
    }

    async fn find_by_email(&self, email: &str) -> autome_core::Result<Option<UserRecord>> {
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                &format!("SELECT {RECORD_COLUMNS} FROM users WHERE email = $1"),
                &[&email],
            )
            .await
            .map_err(|e| storage_err("find_by_email", e))?;
        Ok(row.as_ref().map(record))
    }

    async fn find_by_id(&self, id: &str) -> autome_core::Result<Option<UserRecord>> {
        // `users.id` est un UUID : un `sub` d'une autre forme ne peut désigner
        // aucun compte (et ferait échouer le cast).
        if !is_uuid(id) {
            return Ok(None);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                &format!("SELECT {RECORD_COLUMNS} FROM users WHERE id = $1::text::uuid"),
                &[&id],
            )
            .await
            .map_err(|e| storage_err("find_by_id", e))?;
        Ok(row.as_ref().map(record))
    }

    async fn update_password(
        &self,
        id: &str,
        expected_hash: &str,
        new_hash: &str,
    ) -> autome_core::Result<bool> {
        if !is_uuid(id) {
            return Ok(false);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Écriture conditionnelle sur l'ancien hash : la ligne verrouillée par
        // le premier `UPDATE` ne correspond plus pour le second.
        let n = client
            .execute(
                "UPDATE users SET password_hash = $3, updated_at = now() \
                 WHERE id = $1::text::uuid AND password_hash = $2",
                &[&id, &expected_hash, &new_hash],
            )
            .await
            .map_err(|e| storage_err("update_password", e))?;
        Ok(n == 1)
    }

    async fn list(
        &self,
        query: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> autome_core::Result<UserPage> {
        let pattern = query.map(contains_pattern);
        let (offset, limit) = (
            i64::try_from(offset).unwrap_or(i64::MAX),
            i64::try_from(limit).unwrap_or(i64::MAX),
        );
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Emails normalisés à l'écriture : `LIKE` suffit (pas d'`ILIKE`).
        let filter = "$1::text IS NULL OR u.email LIKE $1";
        let rows = client
            .query(
                &format!(
                    "SELECT {SUMMARY_COLUMNS} FROM users u WHERE {filter} \
                     ORDER BY u.created_at, u.id LIMIT $2 OFFSET $3"
                ),
                &[&pattern, &limit, &offset],
            )
            .await
            .map_err(|e| storage_err("list users", e))?;
        let total: i64 = client
            .query_one(
                &format!("SELECT count(*) FROM users u WHERE {filter}"),
                &[&pattern],
            )
            .await
            .map_err(|e| storage_err("count users", e))?
            .get(0);
        Ok(UserPage {
            items: rows.iter().map(summary).collect(),
            total: usize::try_from(total).unwrap_or(0),
        })
    }

    async fn summary(&self, id: &str) -> autome_core::Result<Option<UserSummary>> {
        if !is_uuid(id) {
            return Ok(None);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let row = client
            .query_opt(
                &format!("SELECT {SUMMARY_COLUMNS} FROM users u WHERE u.id = $1::text::uuid"),
                &[&id],
            )
            .await
            .map_err(|e| storage_err("summary", e))?;
        Ok(row.as_ref().map(summary))
    }

    async fn set_permissions(
        &self,
        id: &str,
        permissions: &HashSet<String>,
    ) -> autome_core::Result<bool> {
        if !is_uuid(id) {
            return Ok(false);
        }
        let mut client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| storage_err("begin", e))?;
        // Verrou de la ligne du compte : deux remplacements concurrents sont
        // sérialisés (sinon leurs DELETE/INSERT pourraient s'entrelacer).
        let exists = tx
            .query_opt(
                "SELECT 1 FROM users WHERE id = $1::text::uuid FOR UPDATE",
                &[&id],
            )
            .await
            .map_err(|e| storage_err("lock user", e))?
            .is_some();
        if !exists {
            return Ok(false);
        }
        tx.execute("DELETE FROM user_permissions WHERE user_id = $1", &[&id])
            .await
            .map_err(|e| storage_err("clear permissions", e))?;
        let granted: Vec<&str> = permissions.iter().map(String::as_str).collect();
        tx.execute(
            "INSERT INTO user_permissions (user_id, permission) \
             SELECT $1, p FROM UNNEST($2::text[]) AS p",
            &[&id, &granted],
        )
        .await
        .map_err(|e| storage_err("grant permissions", e))?;
        tx.commit().await.map_err(|e| storage_err("commit", e))?;
        Ok(true)
    }

    async fn set_disabled(&self, id: &str, disabled: bool) -> autome_core::Result<bool> {
        if !is_uuid(id) {
            return Ok(false);
        }
        let client = self.pool.get().await.map_err(|e| storage_err("pool", e))?;
        // Une nouvelle suspension garde la date de la première.
        let n = client
            .execute(
                "UPDATE users SET \
                   disabled_at = CASE WHEN $2 THEN COALESCE(disabled_at, now()) END, \
                   updated_at = now() \
                 WHERE id = $1::text::uuid",
                &[&id, &disabled],
            )
            .await
            .map_err(|e| storage_err("set_disabled", e))?;
        Ok(n == 1)
    }
}

/// `true` pour un UUID textuel (`8-4-4-4-12` chiffres hexadécimaux).
pub(crate) fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::{contains_pattern, is_uuid};

    #[test]
    fn like_metacharacters_are_escaped() {
        assert_eq!(contains_pattern("alice"), "%alice%");
        assert_eq!(contains_pattern("a_b%c\\d"), "%a\\_b\\%c\\\\d%");
    }

    #[test]
    fn uuid_shape_is_checked_before_querying() {
        assert!(is_uuid("00000000-0000-0000-0000-000000000001"));
        assert!(is_uuid("3F2504E0-4F89-41D3-9A0C-0305E82C3301"));
        assert!(!is_uuid("alice"));
        assert!(!is_uuid("00000000-0000-0000-0000-00000000000"));
        assert!(!is_uuid("00000000_0000-0000-0000-000000000001"));
        assert!(!is_uuid("0000000g-0000-0000-0000-000000000001"));
    }
}

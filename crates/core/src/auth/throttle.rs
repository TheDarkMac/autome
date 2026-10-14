//! Anti-bruteforce de la connexion (backlog auth, #182).
//!
//! Au plus [`MAX_LOGIN_FAILURES`] tentatives sans succès par email et par
//! **fenêtre fixe** de [`LOGIN_WINDOW_SECONDS`] ; au-delà, refus jusqu'à la fin
//! de la fenêtre. Une connexion réussie remet le compteur à zéro.
//!
//! Une tentative est **réservée avant** la vérification du mot de passe, de
//! façon atomique : une rafale de requêtes parallèles — y compris réparties
//! sur plusieurs répliques de l'API avec l'impl PostgreSQL — ne peut pas
//! dépasser le quota en passant le contrôle avant que les premiers échecs
//! soient enregistrés. Les tentatives refusées sont aussi comptées (sans
//! prolonger la fenêtre).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::Result;

/// Tentatives de connexion tolérées par email et par fenêtre.
pub const MAX_LOGIN_FAILURES: u32 = 5;
/// Durée de la fenêtre du compteur (15 min).
pub const LOGIN_WINDOW_SECONDS: i64 = 15 * 60;

/// Verdict d'une réservation de tentative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginAttempt {
    /// Tentative autorisée (le mot de passe peut être vérifié).
    Allowed,
    /// Quota épuisé : réessayer dans `retry_after_secs` secondes.
    Blocked {
        /// Secondes restantes avant la fin de la fenêtre (≥ 1).
        retry_after_secs: i64,
    },
}

impl LoginAttempt {
    /// Verdict à partir du compteur **après** réservation (`attempts`
    /// tentatives dans la fenêtre ouverte à `window_start`).
    pub fn from_counter(attempts: i64, window_start: i64, now: i64) -> Self {
        if attempts <= i64::from(MAX_LOGIN_FAILURES) {
            Self::Allowed
        } else {
            Self::Blocked {
                retry_after_secs: (window_start + LOGIN_WINDOW_SECONDS - now).max(1),
            }
        }
    }
}

/// Compteur de tentatives de connexion, partageable entre répliques.
#[async_trait]
pub trait LoginThrottleStore: Send + Sync {
    /// Réserve atomiquement une tentative pour `email` à `now` (secondes Unix).
    async fn begin_attempt(&self, email: &str, now: i64) -> Result<LoginAttempt>;

    /// Remet le compteur de `email` à zéro (connexion réussie).
    async fn reset(&self, email: &str) -> Result<()>;

    /// Supprime les fenêtres expirées à `now` ; renvoie leur nombre.
    async fn gc(&self, now: i64) -> Result<usize>;
}

/// Au-delà, les fenêtres expirées sont purgées à la volée (borne mémoire face
/// à un attaquant qui varie les emails).
const PRUNE_THRESHOLD: usize = 10_000;

/// Compteur in-process (dev, tests, mono-réplique).
#[derive(Debug, Default)]
pub struct InMemoryLoginThrottle {
    /// email → (tentatives, début de fenêtre en secondes Unix).
    attempts: Mutex<HashMap<String, (i64, i64)>>,
}

impl InMemoryLoginThrottle {
    /// Construit un compteur vide.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (i64, i64)>> {
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl LoginThrottleStore for InMemoryLoginThrottle {
    async fn begin_attempt(&self, email: &str, now: i64) -> Result<LoginAttempt> {
        let mut attempts = self.lock();
        if attempts.len() >= PRUNE_THRESHOLD {
            attempts.retain(|_, (_, start)| now - *start < LOGIN_WINDOW_SECONDS);
        }
        let entry = attempts.entry(email.to_string()).or_insert((0, now));
        if now - entry.1 >= LOGIN_WINDOW_SECONDS {
            *entry = (0, now);
        }
        entry.0 = entry.0.saturating_add(1);
        Ok(LoginAttempt::from_counter(entry.0, entry.1, now))
    }

    async fn reset(&self, email: &str) -> Result<()> {
        self.lock().remove(email);
        Ok(())
    }

    async fn gc(&self, now: i64) -> Result<usize> {
        let mut attempts = self.lock();
        let before = attempts.len();
        attempts.retain(|_, (_, start)| now - *start < LOGIN_WINDOW_SECONDS);
        Ok(before - attempts.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1_700_000_000;

    #[tokio::test]
    async fn blocks_after_the_quota_until_the_window_ends() {
        let t = InMemoryLoginThrottle::new();
        for _ in 0..MAX_LOGIN_FAILURES {
            assert_eq!(
                t.begin_attempt("a", START).await.unwrap(),
                LoginAttempt::Allowed
            );
        }
        assert_eq!(
            t.begin_attempt("a", START + 60).await.unwrap(),
            LoginAttempt::Blocked {
                retry_after_secs: LOGIN_WINDOW_SECONDS - 60
            }
        );
        // Les refus ne prolongent pas la fenêtre.
        assert_eq!(
            t.begin_attempt("a", START + LOGIN_WINDOW_SECONDS)
                .await
                .unwrap(),
            LoginAttempt::Allowed
        );
    }

    #[tokio::test]
    async fn counters_are_per_email_and_reset_on_success() {
        let t = InMemoryLoginThrottle::new();
        for _ in 0..MAX_LOGIN_FAILURES {
            t.begin_attempt("a", START).await.unwrap();
        }
        assert_eq!(
            t.begin_attempt("b", START).await.unwrap(),
            LoginAttempt::Allowed
        );
        t.reset("a").await.unwrap();
        assert_eq!(
            t.begin_attempt("a", START).await.unwrap(),
            LoginAttempt::Allowed
        );
    }

    #[tokio::test]
    async fn gc_removes_only_expired_windows() {
        let t = InMemoryLoginThrottle::new();
        t.begin_attempt("old", START).await.unwrap();
        t.begin_attempt("new", START + LOGIN_WINDOW_SECONDS - 1)
            .await
            .unwrap();
        assert_eq!(t.gc(START + LOGIN_WINDOW_SECONDS).await.unwrap(), 1);
        assert_eq!(t.gc(START + LOGIN_WINDOW_SECONDS).await.unwrap(), 0);
    }

    #[test]
    fn verdict_from_counter_never_asks_to_wait_less_than_a_second() {
        assert_eq!(
            LoginAttempt::from_counter(i64::from(MAX_LOGIN_FAILURES) + 1, START, START + 10_000),
            LoginAttempt::Blocked {
                retry_after_secs: 1
            }
        );
    }
}

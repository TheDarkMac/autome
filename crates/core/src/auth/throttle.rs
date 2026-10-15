//! Anti-bruteforce de l'authentification (#182, #190).
//!
//! Compteurs à **fenêtre fixe** de [`LOGIN_WINDOW_SECONDS`], identifiés par
//! une clé opaque et plafonnés par un quota propre à chaque usage :
//!
//! - `email:<email>` — tentatives de connexion sans succès sur un compte
//!   ([`MAX_LOGIN_FAILURES`]) ; une connexion réussie remet le compteur à zéro ;
//! - `login-ip:…` / `signup-ip:…` (#190) — tentatives par adresse IP cliente,
//!   tous emails confondus (pulvérisation de mots de passe, bourrage
//!   d'identifiants, épuisement CPU par Argon2). La clé est construite par
//!   l'API à partir d'un HMAC de l'adresse : l'IP n'est jamais stockée.
//!
//! Une tentative est **réservée avant** le travail coûteux (vérification du
//! mot de passe), de façon atomique : une rafale de requêtes parallèles — y
//! compris réparties sur plusieurs répliques de l'API avec l'impl PostgreSQL —
//! ne peut pas dépasser le quota. Les tentatives refusées sont aussi comptées
//! (sans prolonger la fenêtre).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::Result;

/// Tentatives de connexion tolérées par email et par fenêtre.
pub const MAX_LOGIN_FAILURES: u32 = 5;
/// Durée de la fenêtre de tous les compteurs (15 min).
pub const LOGIN_WINDOW_SECONDS: i64 = 15 * 60;

/// Clé du compteur des échecs de connexion d'un email (déjà normalisé).
pub fn email_throttle_key(email: &str) -> String {
    format!("email:{email}")
}

/// Verdict d'une réservation de tentative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginAttempt {
    /// Tentative autorisée (le travail coûteux peut être fait).
    Allowed,
    /// Quota épuisé : réessayer dans `retry_after_secs` secondes.
    Blocked {
        /// Secondes restantes avant la fin de la fenêtre (≥ 1).
        retry_after_secs: i64,
    },
}

impl LoginAttempt {
    /// Verdict à partir du compteur **après** réservation (`attempts`
    /// tentatives dans la fenêtre ouverte à `window_start`), pour un quota de
    /// `max_attempts`.
    pub fn from_counter(attempts: i64, max_attempts: u32, window_start: i64, now: i64) -> Self {
        if attempts <= i64::from(max_attempts) {
            Self::Allowed
        } else {
            Self::Blocked {
                retry_after_secs: (window_start + LOGIN_WINDOW_SECONDS - now).max(1),
            }
        }
    }
}

/// Compteurs de tentatives, partageables entre répliques.
#[async_trait]
pub trait LoginThrottleStore: Send + Sync {
    /// Réserve atomiquement une tentative sur `key` à `now` (secondes Unix) ;
    /// refus au-delà de `max_attempts` dans la fenêtre.
    async fn begin_attempt(&self, key: &str, max_attempts: u32, now: i64) -> Result<LoginAttempt>;

    /// Remet le compteur de `key` à zéro (connexion réussie sur un email).
    async fn reset(&self, key: &str) -> Result<()>;

    /// Rend une tentative réservée sur `key` (jamais en dessous de zéro) : une
    /// connexion réussie ne compte pas dans le quota de son IP. N'annule que
    /// sa propre réservation — pas les échecs précédents.
    async fn refund(&self, key: &str) -> Result<()>;

    /// Supprime les fenêtres expirées à `now` ; renvoie leur nombre.
    async fn gc(&self, now: i64) -> Result<usize>;
}

/// Au-delà, les fenêtres expirées sont purgées à la volée (borne mémoire face
/// à un attaquant qui varie les clés).
const PRUNE_THRESHOLD: usize = 10_000;

/// Compteur in-process (dev, tests, mono-réplique).
#[derive(Debug, Default)]
pub struct InMemoryLoginThrottle {
    /// clé → (tentatives, début de fenêtre en secondes Unix).
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
    async fn begin_attempt(&self, key: &str, max_attempts: u32, now: i64) -> Result<LoginAttempt> {
        let mut attempts = self.lock();
        if attempts.len() >= PRUNE_THRESHOLD {
            attempts.retain(|_, (_, start)| now - *start < LOGIN_WINDOW_SECONDS);
        }
        let entry = attempts.entry(key.to_string()).or_insert((0, now));
        if now - entry.1 >= LOGIN_WINDOW_SECONDS {
            *entry = (0, now);
        }
        entry.0 = entry.0.saturating_add(1);
        Ok(LoginAttempt::from_counter(
            entry.0,
            max_attempts,
            entry.1,
            now,
        ))
    }

    async fn reset(&self, key: &str) -> Result<()> {
        self.lock().remove(key);
        Ok(())
    }

    async fn refund(&self, key: &str) -> Result<()> {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.0 = (entry.0 - 1).max(0);
        }
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
    const MAX: u32 = MAX_LOGIN_FAILURES;

    #[tokio::test]
    async fn blocks_after_the_quota_until_the_window_ends() {
        let t = InMemoryLoginThrottle::new();
        for _ in 0..MAX {
            assert_eq!(
                t.begin_attempt("a", MAX, START).await.unwrap(),
                LoginAttempt::Allowed
            );
        }
        assert_eq!(
            t.begin_attempt("a", MAX, START + 60).await.unwrap(),
            LoginAttempt::Blocked {
                retry_after_secs: LOGIN_WINDOW_SECONDS - 60
            }
        );
        // Les refus ne prolongent pas la fenêtre.
        assert_eq!(
            t.begin_attempt("a", MAX, START + LOGIN_WINDOW_SECONDS)
                .await
                .unwrap(),
            LoginAttempt::Allowed
        );
    }

    #[tokio::test]
    async fn counters_are_per_key_and_reset_on_success() {
        let t = InMemoryLoginThrottle::new();
        for _ in 0..MAX {
            t.begin_attempt("a", MAX, START).await.unwrap();
        }
        assert_eq!(
            t.begin_attempt("b", MAX, START).await.unwrap(),
            LoginAttempt::Allowed
        );
        t.reset("a").await.unwrap();
        assert_eq!(
            t.begin_attempt("a", MAX, START).await.unwrap(),
            LoginAttempt::Allowed
        );
    }

    #[tokio::test]
    async fn quota_is_given_per_call() {
        let t = InMemoryLoginThrottle::new();
        for _ in 0..3 {
            t.begin_attempt("ip", 50, START).await.unwrap();
        }
        // Même compteur, quota plus strict : déjà dépassé.
        assert!(matches!(
            t.begin_attempt("ip", 3, START).await.unwrap(),
            LoginAttempt::Blocked { .. }
        ));
        assert_eq!(
            t.begin_attempt("ip", 50, START).await.unwrap(),
            LoginAttempt::Allowed
        );
    }

    #[tokio::test]
    async fn refund_gives_back_one_attempt_but_never_below_zero() {
        let t = InMemoryLoginThrottle::new();
        for _ in 0..2 {
            t.begin_attempt("ip", 2, START).await.unwrap();
        }
        t.refund("ip").await.unwrap();
        assert_eq!(
            t.begin_attempt("ip", 2, START).await.unwrap(),
            LoginAttempt::Allowed,
            "la tentative rendue est de nouveau disponible"
        );
        assert!(matches!(
            t.begin_attempt("ip", 2, START).await.unwrap(),
            LoginAttempt::Blocked { .. }
        ));
        for _ in 0..10 {
            t.refund("ip").await.unwrap();
        }
        t.refund("absent").await.unwrap();
        // Plancher à zéro : pas de crédit accumulé par des remboursements.
        for _ in 0..2 {
            t.begin_attempt("ip", 2, START).await.unwrap();
        }
        assert!(matches!(
            t.begin_attempt("ip", 2, START).await.unwrap(),
            LoginAttempt::Blocked { .. }
        ));
    }

    #[tokio::test]
    async fn gc_removes_only_expired_windows() {
        let t = InMemoryLoginThrottle::new();
        t.begin_attempt("old", MAX, START).await.unwrap();
        t.begin_attempt("new", MAX, START + LOGIN_WINDOW_SECONDS - 1)
            .await
            .unwrap();
        assert_eq!(t.gc(START + LOGIN_WINDOW_SECONDS).await.unwrap(), 1);
        assert_eq!(t.gc(START + LOGIN_WINDOW_SECONDS).await.unwrap(), 0);
    }

    #[test]
    fn verdict_from_counter_never_asks_to_wait_less_than_a_second() {
        assert_eq!(
            LoginAttempt::from_counter(i64::from(MAX) + 1, MAX, START, START + 10_000),
            LoginAttempt::Blocked {
                retry_after_secs: 1
            }
        );
    }

    #[test]
    fn email_key_is_prefixed() {
        assert_eq!(email_throttle_key("a@b.io"), "email:a@b.io");
    }
}

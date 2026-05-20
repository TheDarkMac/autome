//! Retry HTTP avec backoff exponentiel (F4.5.b).
//!
//! [`RetryingClient`] enveloppe un [`HttpClient`] et applique une
//! [`RetryPolicy`] : nombre d'essais, base/plafond de backoff, statuts à
//! retenter. L'horloge est injectée via [`Clock`] pour pouvoir tester sans
//! vrai sleep.
//!
//! L'honoring du header `Retry-After` reste à faire : `HttpResponse`
//! n'expose pas (encore) les headers de réponse — c'est une décision
//! consciente (cf. domaine 7, où l'API publique stream les en-têtes utiles).

use std::time::Duration;

use crate::http::{HttpClient, HttpRequest, HttpResponse};

/// Horloge injectable (pour rendre [`RetryingClient`] testable sans vrai sleep).
pub trait Clock {
    /// Suspend l'exécution courante pendant `dur`.
    fn sleep(&self, dur: Duration);
}

impl<T: Clock + ?Sized> Clock for &T {
    fn sleep(&self, dur: Duration) {
        (**self).sleep(dur);
    }
}

/// Horloge en production basée sur `std::thread::sleep`.
pub struct StdClock;

impl Clock for StdClock {
    fn sleep(&self, dur: Duration) {
        std::thread::sleep(dur);
    }
}

/// Politique de retry.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Nombre total d'essais (≥ 1).
    pub max_attempts: u32,
    /// Délai avant le **premier** retry.
    pub initial_backoff: Duration,
    /// Facteur multiplicatif appliqué après chaque échec.
    pub backoff_multiplier: f64,
    /// Plafond du délai de retry.
    pub max_backoff: Duration,
    /// Statuts HTTP qui déclenchent un retry (ex. 429 + 5xx).
    pub retry_on_status: Vec<u16>,
}

impl RetryPolicy {
    /// Politique recommandée par défaut : 3 essais, `100ms → 200 → 400`
    /// plafonné à 5 s, retry sur `429`/`500`/`502`/`503`/`504`.
    pub fn standard() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            backoff_multiplier: 2.0,
            max_backoff: Duration::from_secs(5),
            retry_on_status: vec![429, 500, 502, 503, 504],
        }
    }
}

/// Transport HTTP qui retente automatiquement selon une [`RetryPolicy`].
pub struct RetryingClient<C: HttpClient, K: Clock> {
    inner: C,
    policy: RetryPolicy,
    clock: K,
}

impl<C: HttpClient, K: Clock> RetryingClient<C, K> {
    /// Construit le client wrapper.
    pub fn new(inner: C, policy: RetryPolicy, clock: K) -> Self {
        Self {
            inner,
            policy,
            clock,
        }
    }
}

impl<C: HttpClient, K: Clock> HttpClient for RetryingClient<C, K> {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
        let max_attempts = self.policy.max_attempts.max(1);
        let mut backoff = self.policy.initial_backoff;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let result = self.inner.send(req);
            let should_retry = match &result {
                Ok(resp) => self.policy.retry_on_status.contains(&resp.status),
                Err(_) => true,
            };
            if !should_retry || attempt >= max_attempts {
                return result;
            }
            self.clock.sleep(backoff);
            backoff =
                Duration::from_secs_f64(backoff.as_secs_f64() * self.policy.backoff_multiplier)
                    .min(self.policy.max_backoff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::testing::RecordingClient;
    use serde_json::json;
    use std::cell::RefCell;

    struct CapturingClock {
        sleeps: RefCell<Vec<Duration>>,
    }
    impl CapturingClock {
        fn new() -> Self {
            Self {
                sleeps: RefCell::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<Duration> {
            self.sleeps.borrow().clone()
        }
    }
    impl Clock for CapturingClock {
        fn sleep(&self, dur: Duration) {
            self.sleeps.borrow_mut().push(dur);
        }
    }

    fn req() -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url: "https://x/y".into(),
            headers: Default::default(),
            body: None,
        }
    }

    #[test]
    fn returns_success_without_sleeping() {
        let inner = RecordingClient::ok(json!({ "ok": true }));
        let clock = CapturingClock::new();
        let r = RetryingClient::new(inner, RetryPolicy::standard(), &clock);
        let out = r.send(&req()).unwrap();
        assert_eq!(out.status, 200);
        assert!(clock.calls().is_empty());
    }

    #[test]
    fn retries_on_transport_error_then_succeeds() {
        let inner = RecordingClient::queue(vec![
            Err("dns".into()),
            Err("dns".into()),
            Ok(HttpResponse {
                status: 200,
                body: json!({ "ok": true }),
            }),
        ]);
        let clock = CapturingClock::new();
        let r = RetryingClient::new(
            inner,
            RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                backoff_multiplier: 2.0,
                max_backoff: Duration::from_secs(1),
                retry_on_status: vec![],
            },
            &clock,
        );
        let out = r.send(&req()).unwrap();
        assert_eq!(out.status, 200);
        // Deux sleeps : avant l'essai 2 puis avant l'essai 3.
        let sleeps = clock.calls();
        assert_eq!(sleeps.len(), 2);
        assert_eq!(sleeps[0], Duration::from_millis(10));
        assert_eq!(sleeps[1], Duration::from_millis(20));
    }

    #[test]
    fn retries_on_429_then_returns_last_failure_when_attempts_exhausted() {
        let inner = RecordingClient::queue(vec![
            Ok(HttpResponse {
                status: 429,
                body: json!({}),
            }),
            Ok(HttpResponse {
                status: 429,
                body: json!({}),
            }),
        ]);
        let clock = CapturingClock::new();
        let r = RetryingClient::new(
            inner,
            RetryPolicy {
                max_attempts: 2,
                initial_backoff: Duration::from_millis(5),
                backoff_multiplier: 2.0,
                max_backoff: Duration::from_secs(1),
                retry_on_status: vec![429],
            },
            &clock,
        );
        let out = r.send(&req()).unwrap();
        assert_eq!(out.status, 429);
        // 1 sleep (entre les deux essais).
        assert_eq!(clock.calls().len(), 1);
    }

    #[test]
    fn no_retry_on_non_retryable_status() {
        let inner = RecordingClient::with_status(404, json!({ "error": "nope" }));
        let clock = CapturingClock::new();
        let r = RetryingClient::new(
            inner,
            RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(1),
                backoff_multiplier: 2.0,
                max_backoff: Duration::from_secs(1),
                retry_on_status: vec![429, 500],
            },
            &clock,
        );
        let out = r.send(&req()).unwrap();
        assert_eq!(out.status, 404);
        assert!(clock.calls().is_empty());
    }

    #[test]
    fn backoff_is_capped_at_max() {
        let inner = RecordingClient::queue(vec![
            Err("x".into()),
            Err("x".into()),
            Err("x".into()),
            Err("x".into()),
        ]);
        let clock = CapturingClock::new();
        let r = RetryingClient::new(
            inner,
            RetryPolicy {
                max_attempts: 4,
                initial_backoff: Duration::from_millis(100),
                backoff_multiplier: 10.0,
                max_backoff: Duration::from_millis(500),
                retry_on_status: vec![],
            },
            &clock,
        );
        let _ = r.send(&req());
        // Backoffs : 100 → 500 (capé, pas 1000) → 500
        let sleeps = clock.calls();
        assert_eq!(sleeps.len(), 3);
        assert_eq!(sleeps[0], Duration::from_millis(100));
        assert_eq!(sleeps[1], Duration::from_millis(500));
        assert_eq!(sleeps[2], Duration::from_millis(500));
    }
}

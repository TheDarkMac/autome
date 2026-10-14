//! Relance bornée des connexions de démarrage (#178).
//!
//! Un broker ou une base déclaré « healthy » par l'orchestrateur n'accepte pas
//! toujours encore les connexions ; un service qui meurt au premier refus part
//! en redémarrages en boucle. [`retry_until`] rejoue une tentative avec un
//! backoff exponentiel, **seulement** pour les erreurs jugées transitoires :
//! une erreur définitive (identifiants, vhost inexistant…) remonte aussitôt.

use std::future::Future;
use std::time::{Duration, Instant};

/// Rejoue `attempt` tant que son erreur est transitoire (`is_transient`) et
/// que `timeout` n'est pas écoulé ; backoff de `first_delay`, doublé jusqu'à
/// `max_delay`. Renvoie la première réussite, sinon la dernière erreur.
pub async fn retry_until<T, E, F, Fut>(
    what: &str,
    timeout: Duration,
    first_delay: Duration,
    max_delay: Duration,
    is_transient: impl Fn(&E) -> bool,
    mut attempt: F,
) -> Result<T, E>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let deadline = Instant::now() + timeout;
    let mut delay = first_delay;
    let mut tries = 1u32;
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(e) if is_transient(&e) && Instant::now() + delay < deadline => {
                tracing::warn!(
                    service = what,
                    attempt = tries,
                    retry_in_ms = delay.as_millis() as u64,
                    error = %e,
                    "connexion impossible, nouvelle tentative"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(max_delay);
                tries += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const MS: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn succeeds_after_transient_failures() {
        let calls = AtomicU32::new(0);
        let got = retry_until(
            "test",
            Duration::from_secs(5),
            MS,
            4 * MS,
            |_: &String| true,
            || async {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 4 {
                    Err(format!("refus n°{n}"))
                } else {
                    Ok(n)
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(got, 4);
    }

    #[tokio::test]
    async fn gives_up_with_the_last_error_after_the_timeout() {
        let calls = AtomicU32::new(0);
        let started = Instant::now();
        let err = retry_until(
            "test",
            Duration::from_millis(50),
            5 * MS,
            10 * MS,
            |_: &String| true,
            || async {
                Err::<(), _>(format!(
                    "refus n°{}",
                    calls.fetch_add(1, Ordering::SeqCst) + 1
                ))
            },
        )
        .await
        .unwrap_err();
        let n = calls.load(Ordering::SeqCst);
        assert!(n >= 2, "plusieurs tentatives : {n}");
        assert_eq!(err, format!("refus n°{n}"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn permanent_errors_are_not_retried() {
        let calls = AtomicU32::new(0);
        let err = retry_until(
            "test",
            Duration::from_secs(5),
            MS,
            MS,
            |e: &String| e.starts_with("transitoire"),
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>("vhost not found".to_string())
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err, "vhost not found");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "aucune relance");
    }
}

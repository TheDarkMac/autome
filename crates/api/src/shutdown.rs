//! Arrêt gracieux de l'API (#194).
//!
//! Sur `SIGTERM` (déploiement progressif, réduction d'échelle, éviction) ou
//! `SIGINT`, l'orchestrateur retire le pod des endpoints du Service **en
//! parallèle** de l'envoi du signal : pendant quelques secondes, l'ingress lui
//! envoie encore du trafic. D'où la séquence :
//!
//! 1. **drain annoncé** : `/readyz` répond 503 `draining` (cf.
//!    [`crate::probes`]) ; `/livez` reste 200 — le pod ne doit pas être tué en
//!    plein arrêt ;
//! 2. **délai de retrait** ([`ShutdownConfig::delay`]) : le serveur continue de
//!    servir normalement, le temps que plus personne ne route vers lui ;
//! 3. **drain** : plus de nouvelles connexions, les requêtes en cours se
//!    terminent, au plus [`ShutdownConfig::timeout`] — au-delà, elles sont
//!    interrompues et le process sort.
//!
//! La période de grâce de l'orchestrateur (k8s `terminationGracePeriodSeconds`,
//! compose `stop_grace_period`) doit dépasser `delay + timeout`.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Délai de retrait par défaut.
pub const DEFAULT_SHUTDOWN_DELAY: Duration = Duration::from_secs(5);
/// Durée maximale du drain par défaut.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

/// Indicateur « arrêt en cours », partagé avec la sonde de readiness.
#[derive(Debug, Clone, Default)]
pub struct Draining(Arc<AtomicBool>);

impl Draining {
    /// Annonce le drain (idempotent).
    pub fn start(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// `true` une fois l'arrêt commencé.
    pub fn is_draining(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Durées de la séquence d'arrêt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownConfig {
    /// Délai de retrait : service normal après l'annonce du drain.
    pub delay: Duration,
    /// Durée maximale du drain des requêtes en cours.
    pub timeout: Duration,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            delay: DEFAULT_SHUTDOWN_DELAY,
            timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }
}

impl ShutdownConfig {
    /// `AUTOME_SHUTDOWN_DELAY_SECS` (défaut 5) et
    /// `AUTOME_SHUTDOWN_TIMEOUT_SECS` (défaut 20), en secondes entières.
    pub fn from_env() -> Result<Self, String> {
        Self::from_source(|name| std::env::var(name).ok())
    }

    /// Comme [`from_env`](Self::from_env), variables lues via `get` (tests).
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let secs = |name: &str, default: Duration| match get(name) {
            None => Ok(default),
            Some(v) if v.trim().is_empty() => Ok(default),
            Some(v) => v
                .trim()
                .parse::<u64>()
                .map(Duration::from_secs)
                .map_err(|_| format!("{name} : nombre de secondes ≥ 0 attendu, obtenu `{v}`")),
        };
        Ok(Self {
            delay: secs("AUTOME_SHUTDOWN_DELAY_SECS", DEFAULT_SHUTDOWN_DELAY)?,
            timeout: secs("AUTOME_SHUTDOWN_TIMEOUT_SECS", DEFAULT_SHUTDOWN_TIMEOUT)?,
        })
    }
}

/// Attend `SIGINT` (Ctrl-C) ou, sous Unix, `SIGTERM`.
pub async fn termination_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM indisponible");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Étapes 1 et 2 : attend `signal`, annonce le drain, puis laisse passer
/// `delay`. Résout quand le serveur doit cesser d'accepter des connexions.
pub async fn begin_shutdown(signal: impl Future<Output = ()>, draining: Draining, delay: Duration) {
    signal.await;
    draining.start();
    tracing::info!(
        delay_ms = delay.as_millis() as u64,
        "arrêt demandé : readiness à 503, retrait du trafic"
    );
    tokio::time::sleep(delay).await;
    tracing::info!("fin du délai de retrait : drain des requêtes en cours");
}

/// Sert `app` (HTTP) sur `listener` jusqu'à `shutdown`, puis draine les
/// requêtes en cours pendant au plus `timeout` (étape 3).
pub async fn serve_until(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    timeout: Duration,
) -> std::io::Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let shutdown = async move {
        shutdown.await;
        let _ = started_tx.send(());
    };
    // Adresse du pair : IP cliente de l'anti-bruteforce (#190).
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown);
    let deadline = async move {
        match started_rx.await {
            Ok(()) => tokio::time::sleep(timeout).await,
            // Serveur terminé sans arrêt demandé : pas d'échéance.
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        result = server => {
            tracing::info!("drain terminé");
            result
        }
        () = deadline => {
            tracing::warn!(
                timeout_ms = timeout.as_millis() as u64,
                "drain incomplet : requêtes encore en cours interrompues"
            );
            Ok(())
        }
    }
}

/// Équivalent HTTPS (rustls) de [`serve_until`] : le handle d'`axum_server`
/// borne lui-même le drain à `timeout`.
pub async fn serve_tls_until(
    addr: SocketAddr,
    tls: axum_server::tls_rustls::RustlsConfig,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    timeout: Duration,
) -> std::io::Result<()> {
    let handle = axum_server::Handle::new();
    let trigger = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        trigger.graceful_shutdown(Some(timeout));
    });
    axum_server::bind_rustls(addr, tls)
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;
    tracing::info!("drain terminé");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::{probes_router, ProbesState};
    use axum::routing::get;
    use std::time::Instant;

    /// Serveur de test : `/slow` (300 ms), `/hang` (30 s) et les sondes.
    async fn start(
        delay: Duration,
        timeout: Duration,
    ) -> (
        SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let draining = Draining::default();
        let app = Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    "done"
                }),
            )
            .route(
                "/hang",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "never"
                }),
            )
            .merge(probes_router(
                ProbesState::new(vec![]).with_draining(draining.clone()),
            ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (signal_tx, signal_rx) = oneshot::channel::<()>();
        let shutdown = begin_shutdown(
            async move {
                let _ = signal_rx.await;
            },
            draining,
            delay,
        );
        let server = tokio::spawn(serve_until(listener, app, shutdown, timeout));
        (addr, signal_tx, server)
    }

    fn client() -> reqwest::Client {
        // Une connexion par requête : on observe l'état du listener.
        reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn in_flight_requests_finish_and_readiness_drains_first() {
        let (addr, signal, server) =
            start(Duration::from_millis(200), Duration::from_secs(5)).await;
        let http = client();
        let base = format!("http://{addr}");

        let ready = http.get(format!("{base}/readyz")).send().await.unwrap();
        assert_eq!(ready.status(), 200);

        // Requête longue en vol, puis signal d'arrêt.
        let slow = tokio::spawn({
            let (http, base) = (http.clone(), base.clone());
            async move { http.get(format!("{base}/slow")).send().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Pendant le délai de retrait : toujours servi, mais « pas prêt ».
        let ready = http.get(format!("{base}/readyz")).send().await.unwrap();
        assert_eq!(ready.status(), 503);
        assert_eq!(
            ready.text().await.unwrap(),
            r#"{"status":"draining","checks":{}}"#
        );
        let live = http.get(format!("{base}/livez")).send().await.unwrap();
        assert_eq!(live.status(), 200, "pas de redémarrage en plein drain");

        // La requête en vol se termine normalement.
        let slow = slow.await.unwrap().unwrap();
        assert_eq!(slow.status(), 200);
        assert_eq!(slow.text().await.unwrap(), "done");

        // Délai écoulé : plus de nouvelles connexions, le serveur s'arrête.
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("serveur arrêté")
            .unwrap()
            .unwrap();
        assert!(http.get(format!("{base}/livez")).send().await.is_err());
    }

    #[tokio::test]
    async fn the_drain_is_bounded_by_the_timeout() {
        let (addr, signal, server) = start(Duration::ZERO, Duration::from_millis(300)).await;
        let http = client();
        let hang =
            tokio::spawn(async move { http.get(format!("http://{addr}/hang")).send().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = Instant::now();
        signal.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("drain borné")
            .unwrap()
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        // La requête interminable est interrompue (le runtime de test la
        // coupe au plus tard en sortant).
        hang.abort();
    }

    #[tokio::test]
    async fn without_signal_the_server_keeps_running() {
        let (addr, _signal, server) = start(Duration::ZERO, Duration::from_millis(10)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!server.is_finished());
        let r = client()
            .get(format!("http://{addr}/livez"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        server.abort();
    }

    #[test]
    fn configuration_from_the_environment() {
        assert_eq!(
            ShutdownConfig::from_source(|_| None).unwrap(),
            ShutdownConfig::default()
        );
        let custom = ShutdownConfig::from_source(|name| match name {
            "AUTOME_SHUTDOWN_DELAY_SECS" => Some("0".into()),
            "AUTOME_SHUTDOWN_TIMEOUT_SECS" => Some(" 45 ".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(custom.delay, Duration::ZERO);
        assert_eq!(custom.timeout, Duration::from_secs(45));
        let err = ShutdownConfig::from_source(|name| {
            (name == "AUTOME_SHUTDOWN_TIMEOUT_SECS").then(|| "-3".into())
        })
        .unwrap_err();
        assert!(err.contains("AUTOME_SHUTDOWN_TIMEOUT_SECS"), "{err}");
    }

    #[test]
    fn draining_is_shared_between_clones() {
        let d = Draining::default();
        let seen_by_probe = d.clone();
        assert!(!seen_by_probe.is_draining());
        d.start();
        d.start();
        assert!(seen_by_probe.is_draining());
    }
}

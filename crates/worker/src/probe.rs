//! Serveur de sondes du worker (#192) : `GET /livez`, `GET /readyz` et
//! `GET /metrics` (#235).
//!
//! Le worker ne sert pas d'API : un serveur HTTP/1.1 minimal suffit (une
//! requête par connexion, réponse puis fermeture), sans tirer de framework web
//! dans ce crate (axum reste confiné à l'API, ADR 0001).
//!
//! - `/livez` : tous les consommateurs tournent ([`PoolLiveness`]) — échec ⇒
//!   l'orchestrateur redémarre le conteneur ;
//! - `/readyz` : PostgreSQL et broker joignables
//!   ([`autome_persistence::run_checks`]) — échec ⇒ « pas prêt » (déploiement
//!   progressif suspendu), sans redémarrage. Pendant l'arrêt (#200) : 503
//!   `draining`, et `/livez` reste 200 (les consommateurs ne bouclent plus) ;
//! - `/metrics` : registre [`WorkerMetrics`] au format texte Prometheus,
//!   servi aussi pendant l'arrêt. Aucun id de run, de workflow ni d'utilisateur
//!   dans les séries (labels bornés), d'où l'absence d'authentification, comme
//!   pour `/metrics` de l'API : le port ne doit pas sortir du réseau interne.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use autome_core::prometheus::CONTENT_TYPE;
use autome_core::HealthCheck;
use autome_persistence::{run_checks, CHECK_TIMEOUT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::liveness::{now_ms, PoolLiveness};
use crate::metrics::{Gauges, WorkerMetrics};
use crate::shutdown::InFlightRuns;

/// Délai laissé au client pour envoyer sa ligne de requête.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Taille maximale lue d'une requête (ligne de requête + en-têtes).
const MAX_REQUEST_BYTES: usize = 4 * 1024;
/// Un consommateur au repos qui n'a pas tourné depuis ce délai est figé : bien
/// au-delà de la pause maximale après erreur de file (5 s) et de la
/// reconnexion au broker (10 s).
pub const STALL_MS: i64 = 60_000;

/// Ce que vérifient les sondes.
pub struct ProbeState {
    liveness: Arc<PoolLiveness>,
    checks: Vec<Arc<dyn HealthCheck>>,
    stall_ms: i64,
    busy_limit_ms: i64,
    check_timeout: Duration,
    draining: Arc<AtomicBool>,
    metrics: Arc<WorkerMetrics>,
    in_flight: Arc<InFlightRuns>,
}

impl ProbeState {
    /// Sondes d'un pool dont un run dure au plus `run_timeout_ms` ; un
    /// consommateur occupé au-delà (plus une marge d'une minute) est figé.
    pub fn new(
        liveness: Arc<PoolLiveness>,
        checks: Vec<Arc<dyn HealthCheck>>,
        run_timeout_ms: i64,
    ) -> Self {
        Self {
            liveness,
            checks,
            stall_ms: STALL_MS,
            busy_limit_ms: run_timeout_ms.saturating_add(STALL_MS),
            check_timeout: CHECK_TIMEOUT,
            draining: Arc::new(AtomicBool::new(false)),
            metrics: Arc::new(WorkerMetrics::new()),
            in_flight: Arc::new(InFlightRuns::default()),
        }
    }

    /// `/metrics` sert `metrics`, avec les runs en cours de `in_flight` (#235).
    pub fn with_metrics(
        mut self,
        metrics: Arc<WorkerMetrics>,
        in_flight: Arc<InFlightRuns>,
    ) -> Self {
        self.metrics = metrics;
        self.in_flight = in_flight;
        self
    }

    /// Arrêt en cours dès que `draining` passe à `true` (#200).
    pub fn with_draining(mut self, draining: Arc<AtomicBool>) -> Self {
        self.draining = draining;
        self
    }

    async fn respond(&self, path: &str) -> (u16, &'static str, String) {
        let draining = self.draining.load(Ordering::Acquire);
        if path == "/metrics" {
            let gauges = Gauges {
                runs_in_flight: self.in_flight.len(),
                draining,
            };
            return (200, CONTENT_TYPE, self.metrics.render(gauges));
        }
        if draining {
            // Les consommateurs ne bouclent plus : ne pas les déclarer figés.
            return match path {
                "/livez" => (200, "text/plain", "ok".into()),
                "/readyz" => (
                    503,
                    "application/json",
                    r#"{"checks":{},"status":"draining"}"#.into(),
                ),
                _ => (404, "text/plain", "not found".into()),
            };
        }
        match path {
            "/livez" => match self
                .liveness
                .check(now_ms(), self.stall_ms, self.busy_limit_ms)
            {
                Ok(()) => (200, "text/plain", "ok".into()),
                Err(stuck) => {
                    tracing::error!(%stuck, "liveness : consommateurs figés");
                    (503, "text/plain", "stalled".into())
                }
            },
            "/readyz" => {
                let readiness = run_checks(&self.checks, self.check_timeout).await;
                let checks: serde_json::Map<String, serde_json::Value> = readiness
                    .checks
                    .iter()
                    .map(|(name, ok)| {
                        (
                            (*name).to_string(),
                            (if *ok { "ok" } else { "error" }).into(),
                        )
                    })
                    .collect();
                let ready = readiness.is_ready();
                let body = serde_json::json!({
                    "status": if ready { "ready" } else { "unavailable" },
                    "checks": checks,
                })
                .to_string();
                (if ready { 200 } else { 503 }, "application/json", body)
            }
            _ => (404, "text/plain", "not found".into()),
        }
    }
}

/// Sert les sondes sur `listener` jusqu'à l'arrêt du process.
pub async fn serve(listener: TcpListener, state: Arc<ProbeState>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move { handle(stream, &state).await });
            }
            Err(e) => {
                tracing::warn!(error = %e, "sonde : connexion refusée");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle(mut stream: TcpStream, state: &ProbeState) {
    let (status, content_type, body) = match read_path(&mut stream).await {
        Some(path) => state.respond(&path).await,
        None => (400, "text/plain", "bad request".into()),
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Service Unavailable",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Chemin d'une requête `GET <chemin> HTTP/1.x` (query string ignorée) ; `None`
/// si la requête est illisible, trop longue, lente ou d'une autre méthode.
async fn read_path(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    let read = async {
        // La ligne de requête suffit ; on lit jusqu'à sa fin.
        while !buf.contains(&b'\n') {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 || buf.len() + n > MAX_REQUEST_BYTES {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Some(())
    };
    tokio::time::timeout(READ_TIMEOUT, read).await.ok()??;
    parse_request_line(&buf)
}

fn parse_request_line(buf: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(buf).ok()?.lines().next()?;
    let mut parts = line.split_whitespace();
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method != "GET" || !version.starts_with("HTTP/1.") {
        return None;
    }
    Some(target.split('?').next().unwrap_or(target).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Switch(AtomicBool);

    #[async_trait]
    impl HealthCheck for Switch {
        fn name(&self) -> &'static str {
            "postgres"
        }
        async fn check(&self) -> autome_core::Result<()> {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(autome_core::Error::Invalid("down".into()))
            }
        }
    }

    async fn request(addr: std::net::SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        out
    }

    async fn start(state: ProbeState) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, Arc::new(state)));
        addr
    }

    #[tokio::test]
    async fn liveness_and_readiness_over_real_http() {
        let liveness = Arc::new(PoolLiveness::new(1, now_ms()));
        let db = Arc::new(Switch(AtomicBool::new(true)));
        let addr = start(ProbeState::new(
            liveness.clone(),
            vec![db.clone()],
            3_600_000,
        ))
        .await;

        let live = request(addr, "GET /livez HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(live.starts_with("HTTP/1.1 200 OK\r\n"), "{live}");
        assert!(live.ends_with("\r\n\r\nok"));

        let ready = request(addr, "GET /readyz?verbose=1 HTTP/1.1\r\n\r\n").await;
        assert!(ready.starts_with("HTTP/1.1 200 OK\r\n"), "{ready}");
        assert!(ready.ends_with(r#"{"checks":{"postgres":"ok"},"status":"ready"}"#));

        // Base perdue : pas prêt, mais vivant.
        db.0.store(false, Ordering::SeqCst);
        let ready = request(addr, "GET /readyz HTTP/1.1\r\n\r\n").await;
        assert!(ready.starts_with("HTTP/1.1 503 "), "{ready}");
        assert!(ready.contains(r#""postgres":"error""#));
        let live = request(addr, "GET /livez HTTP/1.1\r\n\r\n").await;
        assert!(live.starts_with("HTTP/1.1 200 "));
    }

    #[tokio::test]
    async fn a_stalled_consumer_fails_liveness() {
        // Consommateur dont le dernier battement remonte à deux minutes.
        let liveness = Arc::new(PoolLiveness::new(1, now_ms() - 120_000));
        let addr = start(ProbeState::new(liveness, vec![], 3_600_000)).await;
        let live = request(addr, "GET /livez HTTP/1.0\r\n\r\n").await;
        assert!(
            live.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{live}"
        );
        assert!(live.ends_with("stalled"));
    }

    #[tokio::test]
    async fn unknown_paths_and_malformed_requests_are_rejected() {
        let liveness = Arc::new(PoolLiveness::new(1, now_ms()));
        let addr = start(ProbeState::new(liveness, vec![], 3_600_000)).await;
        assert!(request(addr, "GET /nope HTTP/1.1\r\n\r\n")
            .await
            .starts_with("HTTP/1.1 404 "));
        assert!(request(addr, "POST /livez HTTP/1.1\r\n\r\n")
            .await
            .starts_with("HTTP/1.1 400 "));
        assert!(request(addr, "garbage\r\n")
            .await
            .starts_with("HTTP/1.1 400 "));
        let huge = format!("GET /{} HTTP/1.1\r\n", "a".repeat(10_000));
        assert!(request(addr, &huge).await.starts_with("HTTP/1.1 400 "));
    }

    #[tokio::test]
    async fn while_draining_readiness_is_503_and_liveness_stays_up() {
        // Consommateurs arrêtés depuis longtemps : figés sans le drain.
        let liveness = Arc::new(PoolLiveness::new(1, now_ms() - 120_000));
        let draining = Arc::new(AtomicBool::new(true));
        let addr =
            start(ProbeState::new(liveness, vec![], 3_600_000).with_draining(draining)).await;
        let ready = request(addr, "GET /readyz HTTP/1.1\r\n\r\n").await;
        assert!(ready.starts_with("HTTP/1.1 503 "), "{ready}");
        assert!(ready.ends_with(r#"{"checks":{},"status":"draining"}"#));
        let live = request(addr, "GET /livez HTTP/1.1\r\n\r\n").await;
        assert!(live.starts_with("HTTP/1.1 200 "), "{live}");
    }

    #[tokio::test]
    async fn metrics_are_served_in_prometheus_text_format_even_while_draining() {
        use crate::metrics::{DeliveryOutcome, RunOutcome};

        let liveness = Arc::new(PoolLiveness::new(1, now_ms()));
        let metrics = Arc::new(WorkerMetrics::new());
        let in_flight = Arc::new(InFlightRuns::default());
        let draining = Arc::new(AtomicBool::new(false));
        let addr = start(
            ProbeState::new(liveness, vec![], 3_600_000)
                .with_metrics(metrics.clone(), in_flight.clone())
                .with_draining(draining.clone()),
        )
        .await;

        metrics.delivery(DeliveryOutcome::Ack);
        metrics.run_finished(RunOutcome::Succeeded, Duration::from_millis(300));
        let _running = in_flight.track("run-1", 1);
        let res = request(addr, "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;
        let (head, body) = res.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n"));
        assert!(head.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(body.contains("autome_worker_deliveries_total{outcome=\"ack\"} 1\n"));
        assert!(body.contains("autome_worker_runs_total{status=\"succeeded\"} 1\n"));
        assert!(body.contains("autome_worker_runs_in_flight 1\n"));
        assert!(body.contains("autome_worker_draining 0\n"));
        // Aucun identifiant de run dans les séries.
        assert!(!body.contains("run-1"));

        draining.store(true, Ordering::SeqCst);
        let res = request(addr, "GET /metrics HTTP/1.1\r\n\r\n").await;
        assert!(res.starts_with("HTTP/1.1 200 OK\r\n"), "{res}");
        assert!(res.contains("autome_worker_draining 1\n"));
    }

    #[test]
    fn request_line_parsing() {
        assert_eq!(
            parse_request_line(b"GET /readyz?x=1 HTTP/1.1\r\n").as_deref(),
            Some("/readyz")
        );
        assert_eq!(parse_request_line(b"GET /livez HTTP/2\r\n"), None);
        assert_eq!(parse_request_line(b"\xff\xfe"), None);
    }
}

//! Métriques Prometheus de l'API (F10.6.a).
//!
//! Registre **sans état global** (à l'inverse du recorder global de la crate
//! `metrics`) : on instancie un [`Metrics`], on le partage via `Arc` et on
//! l'injecte dans le routeur — exactement comme les `InMemory*Store` du
//! projet. Cela le rend entièrement testable hors serveur.
//!
//! Trois séries sont exposées, suffixées `autome_` :
//! - `autome_http_requests_total{method,path,status}` — counter,
//! - `autome_http_request_duration_seconds{method,path}` — histogram,
//! - `autome_http_requests_in_flight` — gauge.
//!
//! Le label `path` est le **template de route** (`/runs/:run_id`), pas l'URL
//! concrète : on borne ainsi la cardinalité (un `run_id` distinct ne crée pas
//! une série de plus).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{MatchedPath, State},
    http::{header, Request},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

/// Bornes supérieures (`le`) des buckets du histogram de latence, en secondes.
/// Reprend les buckets par défaut de Prometheus (`prometheus.DefBuckets`).
const BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Histogram cumulatif sur les bornes [`BUCKETS`].
///
/// `le_counts[i]` compte les observations `<= BUCKETS[i]` (déjà cumulatif :
/// chaque observation incrémente tous les buckets qui la couvrent). `count`
/// est le total (équivaut au bucket `+Inf`) et `sum` la somme des valeurs.
#[derive(Clone)]
struct Histogram {
    le_counts: Vec<u64>,
    count: u64,
    sum: f64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            le_counts: vec![0; BUCKETS.len()],
            count: 0,
            sum: 0.0,
        }
    }
}

impl Histogram {
    fn observe(&mut self, value: f64) {
        for (i, &bound) in BUCKETS.iter().enumerate() {
            if value <= bound {
                self.le_counts[i] += 1;
            }
        }
        self.count += 1;
        self.sum += value;
    }
}

#[derive(Default)]
struct Inner {
    /// (method, path, status) → nombre de requêtes.
    requests: BTreeMap<(String, String, String), u64>,
    /// (method, path) → histogram des latences.
    durations: BTreeMap<(String, String), Histogram>,
    /// Requêtes en cours de traitement.
    in_flight: i64,
}

/// Registre de métriques HTTP, partageable (`Arc<Metrics>`) et thread-safe.
#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    /// Crée un registre vide.
    pub fn new() -> Self {
        Self::default()
    }

    /// Incrémente le compteur de requêtes en cours.
    pub fn inc_in_flight(&self) {
        self.inner.lock().expect("metrics lock").in_flight += 1;
    }

    /// Décrémente le compteur de requêtes en cours.
    pub fn dec_in_flight(&self) {
        self.inner.lock().expect("metrics lock").in_flight -= 1;
    }

    /// Enregistre une requête terminée : counter `+1` pour la combinaison
    /// (method, path, status) et observation `duration_secs` dans le histogram
    /// (method, path).
    pub fn record(&self, method: &str, path: &str, status: u16, duration_secs: f64) {
        let mut inner = self.inner.lock().expect("metrics lock");
        *inner
            .requests
            .entry((method.to_owned(), path.to_owned(), status.to_string()))
            .or_insert(0) += 1;
        inner
            .durations
            .entry((method.to_owned(), path.to_owned()))
            .or_default()
            .observe(duration_secs);
    }

    /// Rend les métriques au format d'exposition texte Prometheus (v0.0.4).
    pub fn render(&self) -> String {
        let inner = self.inner.lock().expect("metrics lock");
        let mut out = String::new();

        // Gauge in-flight.
        out.push_str("# HELP autome_http_requests_in_flight In-flight HTTP requests.\n");
        out.push_str("# TYPE autome_http_requests_in_flight gauge\n");
        let _ = writeln!(out, "autome_http_requests_in_flight {}", inner.in_flight);

        // Counter total.
        out.push_str("# HELP autome_http_requests_total Total number of HTTP requests.\n");
        out.push_str("# TYPE autome_http_requests_total counter\n");
        for ((method, path, status), count) in &inner.requests {
            let _ = writeln!(
                out,
                "autome_http_requests_total{{method=\"{}\",path=\"{}\",status=\"{}\"}} {}",
                escape(method),
                escape(path),
                escape(status),
                count
            );
        }

        // Histogram latence.
        out.push_str(
            "# HELP autome_http_request_duration_seconds HTTP request latency in seconds.\n",
        );
        out.push_str("# TYPE autome_http_request_duration_seconds histogram\n");
        for ((method, path), hist) in &inner.durations {
            let (m, p) = (escape(method), escape(path));
            for (i, &bound) in BUCKETS.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "autome_http_request_duration_seconds_bucket{{method=\"{}\",path=\"{}\",le=\"{}\"}} {}",
                    m,
                    p,
                    fmt_f64(bound),
                    hist.le_counts[i]
                );
            }
            let _ = writeln!(
                out,
                "autome_http_request_duration_seconds_bucket{{method=\"{}\",path=\"{}\",le=\"+Inf\"}} {}",
                m, p, hist.count
            );
            let _ = writeln!(
                out,
                "autome_http_request_duration_seconds_sum{{method=\"{}\",path=\"{}\"}} {}",
                m,
                p,
                fmt_f64(hist.sum)
            );
            let _ = writeln!(
                out,
                "autome_http_request_duration_seconds_count{{method=\"{}\",path=\"{}\"}} {}",
                m, p, hist.count
            );
        }

        out
    }
}

/// Échappe une valeur de label selon le format Prometheus : `\`, `"` et saut
/// de ligne. Les labels du projet n'en contiennent normalement pas, mais le
/// rendu doit rester valide quoi qu'il arrive.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Formate un flottant sans notation scientifique ni `.0` superflu
/// (`1.0` → `1`, `0.005` → `0.005`), conforme aux valeurs `le` Prometheus.
fn fmt_f64(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        trimmed.to_owned()
    }
}

/// Middleware : mesure chaque requête (in-flight, latence, statut) et la
/// comptabilise dans le registre. Le `path` est le template de route lu dans
/// les extensions ([`MatchedPath`]), avec repli sur le chemin brut.
pub async fn track_metrics(
    State(metrics): State<std::sync::Arc<Metrics>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());
    let method = req.method().clone();

    metrics.inc_in_flight();
    let start = Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    metrics.dec_in_flight();

    metrics.record(method.as_str(), &path, response.status().as_u16(), elapsed);
    response
}

/// Handler de la route d'exposition.
async fn render_handler(State(metrics): State<std::sync::Arc<Metrics>>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        metrics.render(),
    )
}

/// Sous-router exposant `GET /metrics` (à merger au niveau racine).
pub fn metrics_router(metrics: std::sync::Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(render_handler))
        .with_state(metrics)
}

/// Applique le middleware de comptage [`track_metrics`] à `router`, lié au
/// registre `metrics`. Encapsule `from_fn_with_state` pour éviter d'exposer
/// le type opaque de la couche au point d'appel.
pub fn with_metrics(router: Router, metrics: std::sync::Arc<Metrics>) -> Router {
    router.route_layer(middleware::from_fn_with_state(metrics, track_metrics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn render_empty_has_only_headers_and_gauge() {
        let m = Metrics::new();
        let out = m.render();
        assert!(out.contains("# TYPE autome_http_requests_in_flight gauge"));
        assert!(out.contains("autome_http_requests_in_flight 0"));
        // Pas de ligne de série counter/histogram tant qu'aucune requête.
        assert!(!out.contains("autome_http_requests_total{"));
        assert!(!out.contains("autome_http_request_duration_seconds_bucket"));
    }

    #[test]
    fn counter_increments_per_combination() {
        let m = Metrics::new();
        m.record("GET", "/health", 200, 0.001);
        m.record("GET", "/health", 200, 0.002);
        m.record("POST", "/health", 500, 0.5);
        let out = m.render();
        assert!(out.contains(
            "autome_http_requests_total{method=\"GET\",path=\"/health\",status=\"200\"} 2"
        ));
        assert!(out.contains(
            "autome_http_requests_total{method=\"POST\",path=\"/health\",status=\"500\"} 1"
        ));
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let m = Metrics::new();
        // 0.002 ≤ 0.005 ; 0.2 tombe au-delà de 0.1 mais ≤ 0.25.
        m.record("GET", "/x", 200, 0.002);
        m.record("GET", "/x", 200, 0.2);
        let out = m.render();
        // le=0.005 ne couvre que la 1re observation.
        assert!(out.contains(
            "autome_http_request_duration_seconds_bucket{method=\"GET\",path=\"/x\",le=\"0.005\"} 1"
        ));
        // le=0.25 couvre les deux.
        assert!(out.contains(
            "autome_http_request_duration_seconds_bucket{method=\"GET\",path=\"/x\",le=\"0.25\"} 2"
        ));
        // +Inf == count == 2.
        assert!(out.contains(
            "autome_http_request_duration_seconds_bucket{method=\"GET\",path=\"/x\",le=\"+Inf\"} 2"
        ));
        assert!(out
            .contains("autome_http_request_duration_seconds_count{method=\"GET\",path=\"/x\"} 2"));
        // sum = 0.202.
        assert!(out.contains(
            "autome_http_request_duration_seconds_sum{method=\"GET\",path=\"/x\"} 0.202"
        ));
    }

    #[test]
    fn in_flight_gauge_goes_up_and_down() {
        let m = Metrics::new();
        m.inc_in_flight();
        m.inc_in_flight();
        m.dec_in_flight();
        assert!(m.render().contains("autome_http_requests_in_flight 1"));
    }

    #[test]
    fn labels_are_escaped() {
        let m = Metrics::new();
        m.record("GET", "/a\"b\\c", 200, 0.01);
        let out = m.render();
        assert!(out.contains("path=\"/a\\\"b\\\\c\""));
    }

    #[test]
    fn fmt_f64_drops_trailing_zeros() {
        assert_eq!(fmt_f64(1.0), "1");
        assert_eq!(fmt_f64(0.005), "0.005");
        assert_eq!(fmt_f64(2.5), "2.5");
        assert_eq!(fmt_f64(0.202), "0.202");
    }

    #[tokio::test]
    async fn metrics_endpoint_renders_after_traffic() {
        use axum::http::StatusCode;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let metrics = Arc::new(Metrics::new());
        let app = with_metrics(
            Router::new()
                .route("/ping", get(|| async { "pong" }))
                .merge(metrics_router(metrics.clone())),
            metrics.clone(),
        );

        // Une requête sur /ping doit être comptabilisée.
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Scrape /metrics.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        // Le template de route est utilisé comme label `path`.
        assert!(text.contains(
            "autome_http_requests_total{method=\"GET\",path=\"/ping\",status=\"200\"} 1"
        ));
        assert!(text.contains(
            "autome_http_request_duration_seconds_count{method=\"GET\",path=\"/ping\"} 1"
        ));
    }
}

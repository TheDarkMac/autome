//! Traçage des requêtes HTTP (#176, #249).
//!
//! Chaque requête s'exécute dans un span `http_request` (`request_id`,
//! `method`, `path`) — tous les logs émis par les handlers et les stores
//! pendant la requête le portent — et produit une **ligne d'accès** (statut,
//! durée). L'identifiant de corrélation vient de l'en-tête `x-request-id` du
//! client ou du proxy s'il est sûr à journaliser, sinon il est généré ; il est
//! renvoyé dans la réponse pour qu'un utilisateur puisse le citer.
//!
//! Le chemin est journalisé **sans la query string** (elle peut porter des
//! données sensibles, ex. `code` OAuth).
//!
//! **Traces** (#249). `http_request` enveloppe le routage, son nom ne peut
//! pas porter la route : il reste dans les logs, et une requête routée ouvre
//! dans la trace un span serveur `http_server` nommé `{méthode} {route}`
//! (`POST /workflows/:id/runs` — gabarit, jamais d'id ni de token). Les sondes et
//! `/metrics` n'en ouvrent pas. Le `traceparent` entrant n'est repris que
//! d'un proxy de confiance (`AUTOME_TRUSTED_PROXIES`, comme
//! `X-Forwarded-For`) : un client direct ne choisit ni sa trace ni son
//! échantillonnage. Avec l'export actif, `trace_id` s'ajoute aux champs de
//! `http_request`, donc aux logs de la requête.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Instant;

use autome_core::telemetry::{self, HTTP_SERVER_SPAN, REQUEST_SPAN, TRACE_CONTEXT_HEADERS};
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use tracing::Instrument;

use crate::client_ip::ClientIpPolicy;

/// En-tête de corrélation, en requête comme en réponse.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Longueur maximale acceptée pour un `x-request-id` entrant.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Routes jamais tracées : sondes et métriques, appelées en continu par
/// l'orchestrateur et Prometheus.
const UNTRACED_ROUTES: [&str; 4] = ["/livez", "/health", "/readyz", "/metrics"];

/// `true` si `id` peut être repris tel quel : non vide, borné, et limité à un
/// alphabet sûr (pas d'injection de lignes ou de caractères de contrôle dans
/// les logs).
fn is_acceptable_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Applique le traçage à **toutes** les requêtes du routeur, y compris les 404
/// (couche externe, contrairement aux métriques posées en `route_layer`), et
/// le span de trace aux requêtes routées. `proxies` décide si le
/// `traceparent` entrant est repris.
pub fn with_request_tracing(router: Router, proxies: ClientIpPolicy) -> Router {
    router
        .route_layer(middleware::from_fn_with_state(proxies, trace_route))
        .layer(middleware::from_fn(trace_request))
}

async fn trace_request(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|id| is_acceptable_request_id(id))
        .map(str::to_string)
        .unwrap_or_else(telemetry::new_request_id);
    let span = tracing::info_span!(
        REQUEST_SPAN,
        request_id = %request_id,
        method = %request.method(),
        // Le token d'une URL de webhook est un secret (#202).
        path = %crate::webhooks::redact_path(request.uri().path()),
        // Renseigné par `trace_route` quand l'export est actif (#249).
        trace_id = tracing::field::Empty,
    );

    let started = Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;
    let status = response.status().as_u16();
    let duration_ms = started.elapsed().as_millis() as u64;
    span.in_scope(|| {
        if status >= 500 {
            tracing::error!(status, duration_ms, "requête traitée");
        } else {
            tracing::info!(status, duration_ms, "requête traitée");
        }
    });

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// Span serveur de la trace pour une requête routée (cf. module).
async fn trace_route(
    State(proxies): State<ClientIpPolicy>,
    request: Request,
    next: Next,
) -> Response {
    let Some(route) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .filter(|route| !UNTRACED_ROUTES.contains(&route.as_str()))
    else {
        return next.run(request).await;
    };
    let method = request.method().clone();
    let span = tracing::info_span!(
        HTTP_SERVER_SPAN,
        otel.name = %format!("{method} {route}"),
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.request.method = %method,
        http.route = %route,
        http.response.status_code = tracing::field::Empty,
    );
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    if peer.is_some_and(|ip| proxies.trusts(ip)) {
        let carrier: BTreeMap<String, String> = TRACE_CONTEXT_HEADERS
            .iter()
            .filter_map(|name| {
                let value = request.headers().get(*name)?.to_str().ok()?;
                Some((name.to_string(), value.to_string()))
            })
            .collect();
        telemetry::set_parent(&span, &carrier);
    }
    // Le span courant est encore `http_request` : ses logs portent l'id de
    // la trace.
    let trace_id = telemetry::trace_id(&span);
    if let Some(trace_id) = &trace_id {
        tracing::Span::current().record("trace_id", trace_id.as_str());
    }

    let response = next.run(request).instrument(span.clone()).await;
    let status = response.status().as_u16();
    span.record("http.response.status_code", status);
    if status >= 500 {
        span.record("otel.status_code", "error");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn app() -> Router {
        with_request_tracing(
            Router::new().route("/ping", get(|| async { "pong" })),
            ClientIpPolicy::default(),
        )
    }

    async fn request_id_of(request: HttpRequest<Body>) -> (StatusCode, String) {
        let response = app().oneshot(request).await.unwrap();
        let id = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("x-request-id en réponse")
            .to_str()
            .unwrap()
            .to_string();
        (response.status(), id)
    }

    #[tokio::test]
    async fn generates_a_request_id_when_absent() {
        let (status, id) =
            request_id_of(HttpRequest::get("/ping").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn echoes_a_valid_incoming_request_id() {
        let req = HttpRequest::get("/ping")
            .header(REQUEST_ID_HEADER, "edge-7f3a:req_01.b")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_id_of(req).await.1, "edge-7f3a:req_01.b");
    }

    #[tokio::test]
    async fn replaces_unsafe_or_oversized_request_ids() {
        for bad in [
            "a b",
            "id;drop",
            "%0d%0a",
            &"a".repeat(MAX_REQUEST_ID_LEN + 1),
        ] {
            let req = HttpRequest::get("/ping")
                .header(REQUEST_ID_HEADER, bad)
                .body(Body::empty())
                .unwrap();
            let id = request_id_of(req).await.1;
            assert_ne!(id, bad);
            assert_eq!(id.len(), 32, "{bad:?} remplacé par un id généré");
        }
    }

    #[tokio::test]
    async fn unmatched_routes_are_traced_too() {
        let (status, id) =
            request_id_of(HttpRequest::get("/nope").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(id.len(), 32);
    }

    // ─────────────────────────── Traces (#249) ───────────────────────────

    use autome_core::telemetry::{subscriber, LogFormat};
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT_ID: &str = "00f067aa0ba902b7";

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Envoie `request` au routeur `/ping` + `/metrics` (pair `peer`, proxys
    /// de confiance `trusted`) sous un subscriber qui exporte en mémoire ;
    /// renvoie les spans exportés et les lignes de log.
    async fn traced(
        trusted: &str,
        peer: [u8; 4],
        request: HttpRequest<Body>,
    ) -> (Vec<SpanData>, Vec<serde_json::Value>) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let buffer = Buffer::default();
        let _default = tracing::subscriber::set_default(subscriber(
            LogFormat::Json,
            tracing_subscriber::EnvFilter::new("info"),
            buffer.clone(),
            Some(provider.tracer("test")),
        ));
        let app = with_request_tracing(
            Router::new()
                .route("/ping/:id", get(|| async { "pong" }))
                .route("/metrics", get(|| async { "" })),
            ClientIpPolicy::parse(trusted).unwrap(),
        );
        let mut request = request;
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((peer, 4242))));
        app.oneshot(request).await.unwrap();
        let logs = String::from_utf8(buffer.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (exporter.get_finished_spans().unwrap(), logs)
    }

    fn ping_with_traceparent() -> HttpRequest<Body> {
        HttpRequest::get("/ping/42")
            .header("traceparent", format!("00-{TRACE_ID}-{PARENT_ID}-01"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn routed_requests_get_a_server_span_named_after_the_route() {
        let (spans, logs) = traced("", [203, 0, 113, 7], ping_with_traceparent()).await;
        assert_eq!(spans.len(), 1, "seul `http_server` est exporté : {spans:?}");
        let span = &spans[0];
        assert_eq!(span.name, "GET /ping/:id", "gabarit, pas l'id");
        assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Server);
        // Client direct : son `traceparent` est ignoré, nouvelle trace.
        assert_ne!(span.span_context.trace_id().to_string(), TRACE_ID);
        assert_eq!(span.parent_span_id, opentelemetry::trace::SpanId::INVALID);
        let status = span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "http.response.status_code")
            .map(|kv| kv.value.to_string());
        assert_eq!(status.as_deref(), Some("200"));
        // La ligne d'accès renvoie à la trace.
        let access = logs.last().expect("ligne d'accès");
        assert_eq!(
            access["span"]["trace_id"],
            span.span_context.trace_id().to_string().as_str()
        );
    }

    #[tokio::test]
    async fn incoming_trace_context_is_honoured_from_trusted_proxies_only() {
        let (spans, _) = traced("10.0.0.0/8", [10, 1, 2, 3], ping_with_traceparent()).await;
        let span = &spans[0];
        assert_eq!(span.span_context.trace_id().to_string(), TRACE_ID);
        assert_eq!(span.parent_span_id.to_string(), PARENT_ID);
    }

    #[tokio::test]
    async fn probes_metrics_and_unmatched_routes_are_not_traced() {
        for path in ["/metrics", "/nope"] {
            let request = HttpRequest::get(path).body(Body::empty()).unwrap();
            let (spans, logs) = traced("", [203, 0, 113, 7], request).await;
            assert!(spans.is_empty(), "{path} : {spans:?}");
            assert!(!logs.is_empty(), "{path} reste journalisé");
        }
    }
}

//! Traçage des requêtes HTTP (#176).
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

use std::time::Instant;

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use tracing::Instrument;

/// En-tête de corrélation, en requête comme en réponse.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Longueur maximale acceptée pour un `x-request-id` entrant.
const MAX_REQUEST_ID_LEN: usize = 128;

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
/// (couche externe, contrairement aux métriques posées en `route_layer`).
pub fn with_request_tracing(router: Router) -> Router {
    router.layer(middleware::from_fn(trace_request))
}

async fn trace_request(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|id| is_acceptable_request_id(id))
        .map(str::to_string)
        .unwrap_or_else(autome_core::telemetry::new_request_id);
    let span = tracing::info_span!(
        "http_request",
        request_id = %request_id,
        method = %request.method(),
        // Le token d'une URL de webhook est un secret (#202).
        path = %crate::webhooks::redact_path(request.uri().path()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn app() -> Router {
        with_request_tracing(Router::new().route("/ping", get(|| async { "pong" })))
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
}

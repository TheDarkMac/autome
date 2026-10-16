//! Sondes de l'orchestrateur (#192) : **vivant** et **prêt** sont deux
//! questions différentes.
//!
//! | Path      | Question                          | Échec ⇒ (k8s)                     |
//! |-----------|-----------------------------------|-----------------------------------|
//! | `/livez`  | Le process répond-il ?            | redémarrage du conteneur          |
//! | `/readyz` | Ses dépendances sont-elles là ?   | retrait du Service, sans redémarrage |
//! | `/health` | alias de `/livez` (compatibilité) |                                   |
//!
//! La liveness ne dépend d'**aucune** dépendance : une panne de PostgreSQL
//! ferait sinon redémarrer en boucle tous les pods, sans rien réparer. La
//! readiness vérifie PostgreSQL et le broker, en parallèle et bornés en temps
//! ([`autome_persistence::run_checks`]) ; elle ne publie que le nom des
//! vérifications en échec — le détail est journalisé. Pendant l'arrêt
//! gracieux (#194), elle répond 503 `draining` sans rien vérifier.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use autome_core::HealthCheck;
use autome_persistence::{run_checks, CHECK_TIMEOUT};
use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::shutdown::Draining;

/// Vérifications exécutées par `/readyz`.
#[derive(Clone)]
pub struct ProbesState {
    checks: Arc<Vec<Arc<dyn HealthCheck>>>,
    timeout: Duration,
    draining: Draining,
}

impl ProbesState {
    /// Sondes sur `checks`, chacune bornée par [`CHECK_TIMEOUT`].
    pub fn new(checks: Vec<Arc<dyn HealthCheck>>) -> Self {
        Self {
            checks: Arc::new(checks),
            timeout: CHECK_TIMEOUT,
            draining: Draining::default(),
        }
    }

    /// Readiness à 503 dès que `draining` annonce l'arrêt (#194).
    pub fn with_draining(mut self, draining: Draining) -> Self {
        self.draining = draining;
        self
    }

    /// Délai d'une vérification (tests).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Réponse de `/readyz`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReadyzResponse {
    /// `ready`, `unavailable` ou `draining` (arrêt en cours).
    pub status: &'static str,
    /// Par vérification (`postgres`, `rabbitmq`) : `ok` ou `error`.
    pub checks: BTreeMap<&'static str, &'static str>,
}

/// `GET /livez` — le process répond (aucune dépendance vérifiée).
#[utoipa::path(
    get,
    path = "/livez",
    tag = "probes",
    responses((status = 200, description = "Vivant", body = String))
)]
pub async fn livez() -> &'static str {
    "ok"
}

/// `GET /readyz` — prêt à servir : PostgreSQL et broker joignables.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "probes",
    responses(
        (status = 200, description = "Prêt", body = ReadyzResponse),
        (status = 503, description = "Une dépendance au moins est indisponible, ou arrêt en cours", body = ReadyzResponse)
    )
)]
pub async fn readyz(State(state): State<ProbesState>) -> (StatusCode, Json<ReadyzResponse>) {
    if state.draining.is_draining() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadyzResponse {
                status: "draining",
                checks: BTreeMap::new(),
            }),
        );
    }
    let readiness = run_checks(&state.checks, state.timeout).await;
    let checks = readiness
        .checks
        .iter()
        .map(|(name, ok)| (*name, if *ok { "ok" } else { "error" }))
        .collect();
    if readiness.is_ready() {
        (
            StatusCode::OK,
            Json(ReadyzResponse {
                status: "ready",
                checks,
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadyzResponse {
                status: "unavailable",
                checks,
            }),
        )
    }
}

/// Routes publiques `/livez`, `/readyz` et `/health` (alias de `/livez`).
pub fn probes_router(state: ProbesState) -> Router {
    Router::new()
        .route("/livez", get(livez))
        .route("/health", get(livez))
        .route("/readyz", get(readyz))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt;

    /// Dépendance qu'on peut faire tomber et remonter.
    struct Switch(&'static str, AtomicBool);

    #[async_trait]
    impl HealthCheck for Switch {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn check(&self) -> autome_core::Result<()> {
            if self.1.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(autome_core::Error::Invalid(
                    "connexion refusée par 10.0.0.3:5432".into(),
                ))
            }
        }
    }

    async fn get(app: &Router, path: &str) -> (StatusCode, String) {
        let r = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = r.status();
        let body = r.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn readiness_follows_dependencies_while_liveness_does_not() {
        let pg = Arc::new(Switch("postgres", AtomicBool::new(true)));
        let mq = Arc::new(Switch("rabbitmq", AtomicBool::new(true)));
        let app = probes_router(ProbesState::new(vec![pg.clone(), mq.clone()]));

        let (status, body) = get(&app, "/readyz").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "ready");
        assert_eq!(v["checks"]["postgres"], "ok");

        // Panne de base : plus prêt, mais toujours vivant (pas de redémarrage).
        pg.1.store(false, Ordering::SeqCst);
        let (status, body) = get(&app, "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "unavailable");
        assert_eq!(v["checks"]["postgres"], "error");
        assert_eq!(v["checks"]["rabbitmq"], "ok");
        assert!(!body.contains("10.0.0.3"), "pas de détail interne exposé");
        for path in ["/livez", "/health"] {
            assert_eq!(get(&app, path).await, (StatusCode::OK, "ok".to_string()));
        }

        // Retour de la base : de nouveau prêt.
        pg.1.store(true, Ordering::SeqCst);
        assert_eq!(get(&app, "/readyz").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn without_dependencies_the_process_is_ready() {
        let app = probes_router(ProbesState::new(vec![]));
        let (status, body) = get(&app, "/readyz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"status":"ready","checks":{}}"#);
    }
}

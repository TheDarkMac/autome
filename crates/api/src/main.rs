//! `autome-api` — binaire du serveur HTTP (axum).
//!
//! Conformément à l'ADR 0001, axum est confiné à ce crate ; `core` et
//! `engine` n'en dépendent pas. Squelette F1.3.d : un routeur avec une
//! sonde de santé. Le CRUD workflows et le déclenchement de runs relèvent
//! du domaine 7 (F7.*). Le middleware d'authentification (F6.1.c) vit dans
//! [`autome_api::auth`].

use std::sync::Arc;

use autome_api::auth::{protected_router, AuthState};
use autome_api::metrics::{metrics_router, with_metrics, Metrics};
use autome_api::runtime::{build_auth_state, PersistenceMode};
use autome_api::tls::load_rustls_config;
use autome_core::{Config, Profile};
use axum::{routing::get, Router};

/// Handler de la sonde de santé.
async fn health() -> &'static str {
    "ok"
}

/// Construit le routeur racine — `/health` + `/metrics` publics, `/auth/*`
/// protégé par JWT. Toutes les routes sont instrumentées par le middleware de
/// métriques (F10.6.a).
///
/// Isolé de `main` pour être testable sans démarrer de serveur.
fn build_router(auth: AuthState, metrics: Arc<Metrics>) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .nest("/auth", protected_router(auth))
        .merge(metrics_router(metrics.clone()));
    with_metrics(router, metrics)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;

    // Backend de persistance (dette #107) : `AUTOME_PERSISTENCE=postgres` adosse
    // les stores d'auth à PostgreSQL ; défaut `memory` (dev/tests/single-node).
    // Le secret JWT vient toujours de `config.jwt_secret` (constat 1).
    let mode = PersistenceMode::from_env()?;
    let auth = build_auth_state(&config, mode)?;
    println!("autome-api : persistance = {mode:?}");

    // Garde prod (dette #107, constat 4) : en in-memory, l'état (révocations/
    // permissions/audit) n'est ni persisté ni partagé entre répliques. On le
    // signale fort plutôt que de refuser le démarrage (l'image prod resterait
    // bootable). En prod, positionner `AUTOME_PERSISTENCE=postgres`.
    if config.profile == Profile::Prod && mode == PersistenceMode::Memory {
        eprintln!(
            "AVERTISSEMENT (prod) : stores in-memory — état non persisté ni \
             partagé entre répliques. Positionnez AUTOME_PERSISTENCE=postgres \
             (cf. dette #107)."
        );
    }

    let metrics = Arc::new(Metrics::new());
    let app = build_router(auth, metrics);

    // F6.4 — HTTPS via rustls si `AUTOME_TLS_CERT` + `AUTOME_TLS_KEY`
    // sont positionnées ; sinon HTTP en clair. Décision all-or-nothing
    // (pas de fallback silencieux qui dégraderait la sécurité).
    let tls_cert = std::env::var("AUTOME_TLS_CERT").ok();
    let tls_key = std::env::var("AUTOME_TLS_KEY").ok();
    match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let tls_config = load_rustls_config(&cert, &key).await?;
            let addr: std::net::SocketAddr = config.http_addr.parse()?;
            println!(
                "autome-api ({:?}) en écoute sur https://{}  (TLS rustls)",
                config.profile, addr
            );
            axum_server::bind_rustls(addr, tls_config)
                .serve(app.into_make_service())
                .await?;
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(&config.http_addr).await?;
            println!(
                "autome-api ({:?}) en écoute sur http://{}  (sans TLS)",
                config.profile, config.http_addr
            );
            axum::serve(listener, app).await?;
        }
        _ => {
            return Err(
                "TLS partiellement configuré : positionnez AUTOME_TLS_CERT *et* AUTOME_TLS_KEY"
                    .into(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_returns_ok() {
        assert_eq!(health().await, "ok");
    }

    #[test]
    fn router_builds() {
        // Le routeur se construit sans panique (route /health + /auth/*),
        // avec un `AuthState` dérivé d'une config de dev.
        let config = Config::load(|_| None).expect("config dev par défaut");
        let auth = AuthState::from_config(&config);
        let _app: Router = build_router(auth, Arc::new(Metrics::new()));
    }
}

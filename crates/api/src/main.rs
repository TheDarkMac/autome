//! `autome-api` — binaire du serveur HTTP (axum).
//!
//! Conformément à l'ADR 0001, axum est confiné à ce crate ; `core` et
//! `engine` n'en dépendent pas. Squelette F1.3.d : un routeur avec une
//! sonde de santé. Le CRUD workflows et le déclenchement de runs relèvent
//! du domaine 7 (F7.*). Le middleware d'authentification (F6.1.c) vit dans
//! [`autome_api::auth`].

use std::sync::Arc;

use autome_api::auth::protected_router;
use autome_api::credentials::{credentials_router, CredentialsState};
use autome_api::metrics::{metrics_router, with_metrics, Metrics};
use autome_api::runs::{runs_router, RunsState};
use autome_api::runtime::{
    build_backends, migrate, migrate_on_boot_requested, Backends, PersistenceMode,
};
use autome_api::tls::load_rustls_config;
use autome_api::workflows::{
    create_workflow, delete_workflow, get_workflow, list_workflows, update_workflow, WorkflowsState,
};
use autome_core::{Config, Profile};
use axum::{routing::get, Router};

/// Handler de la sonde de santé.
async fn health() -> &'static str {
    "ok"
}

/// Construit le routeur racine : `/health` + `/metrics` publics, `/auth/*`
/// protégé par JWT, et le CRUD `workflows` + `runs` (domaine 7). Toutes les
/// routes sont instrumentées par le middleware de métriques (F10.6.a).
///
/// Les routes workflows sont posées **à plat** (`/workflows`, `/workflows/:id`)
/// plutôt que `nest`ées : le catch-all d'un nest entrerait en conflit avec
/// `/workflows/:id/runs` du router runs.
///
/// Isolé de `main` pour être testable sans démarrer de serveur.
fn build_router(backends: &Backends, metrics: Arc<Metrics>) -> Router {
    let wf_state = WorkflowsState::new(backends.auth.clone(), backends.workflows.clone());
    let runs_state = RunsState::new(
        backends.auth.clone(),
        backends.workflows.clone(),
        backends.runs.clone(),
        backends.dispatcher.clone(),
    );

    let workflows_routes = Router::new()
        .route("/workflows", get(list_workflows).post(create_workflow))
        .route(
            "/workflows/:id",
            get(get_workflow)
                .put(update_workflow)
                .delete(delete_workflow),
        )
        .with_state(wf_state);

    let credentials_state =
        CredentialsState::new(backends.auth.clone(), backends.credentials.clone());

    let router = Router::new()
        .route("/health", get(health))
        .nest("/auth", protected_router(backends.auth.clone()))
        .merge(workflows_routes)
        .merge(runs_router(runs_state))
        .merge(credentials_router(credentials_state))
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

    // Runner de migrations (dette #107) : en mode `postgres` avec
    // `AUTOME_MIGRATE=1`, applique les migrations manquantes au démarrage
    // (idempotent, sérialisé entre répliques par un verrou applicatif).
    if mode == PersistenceMode::Postgres && migrate_on_boot_requested() {
        let applied = migrate(&config).await?;
        println!("autome-api : migrations appliquées = {applied}");
    }

    let backends = build_backends(&config, mode)?;
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
    let app = build_router(&backends, metrics);

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
        // Le routeur racine complet (health + auth + workflows + runs +
        // metrics) se construit sans panique ni conflit de routes, avec des
        // backends in-memory dérivés d'une config de dev.
        let config = Config::load(|_| None).expect("config dev par défaut");
        let backends =
            build_backends(&config, PersistenceMode::Memory).expect("backends in-memory");
        let _app: Router = build_router(&backends, Arc::new(Metrics::new()));
    }
}

//! `autome-api` — binaire du serveur HTTP (axum).
//!
//! Conformément à l'ADR 0001, axum est confiné à ce crate ; `core` et
//! `engine` n'en dépendent pas. Squelette F1.3.d : un routeur avec une
//! sonde de santé. Le CRUD workflows et le déclenchement de runs relèvent
//! du domaine 7 (F7.*). Le middleware d'authentification (F6.1.c) vit dans
//! [`autome_api::auth`].

use std::net::SocketAddr;
use std::sync::Arc;

use autome_api::accounts::{accounts_router, AccountsState, IpThrottle};
use autome_api::auth::protected_router;
use autome_api::credentials::{credentials_router, CredentialsState};
use autome_api::metrics::{metrics_router, with_metrics, Metrics};
use autome_api::observability::with_request_tracing;
use autome_api::runs::{runs_router, RunsState};
use autome_api::runtime::{
    build_backends, build_queue, ensure_durable_backends, migrate, migrate_on_boot_requested,
    purge_expired, Backends, PersistenceMode, QueueMode,
};
use autome_api::tls::load_rustls_config;
use autome_api::workflows::{
    create_workflow, delete_workflow, get_workflow, list_workflows, update_workflow, WorkflowsState,
};
use autome_core::{telemetry, Argon2PasswordHasher, Config};
use axum::{routing::get, Router};

/// Période de la purge des entrées d'authentification expirées.
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// Handler de la sonde de santé.
async fn health() -> &'static str {
    "ok"
}

/// Construit le routeur racine : `/health` + `/metrics` publics,
/// `/auth/signup|login|refresh` publics (émission des jetons), `/auth/whoami|admin`
/// protégés par JWT, et le CRUD `workflows` + `runs` (domaine 7). Toutes les
/// routes sont instrumentées par le middleware de métriques (F10.6.a).
///
/// Les routes workflows sont posées **à plat** (`/workflows`, `/workflows/:id`)
/// plutôt que `nest`ées : le catch-all d'un nest entrerait en conflit avec
/// `/workflows/:id/runs` du router runs.
///
/// Isolé de `main` pour être testable sans démarrer de serveur.
fn build_router(backends: &Backends, accounts: AccountsState, metrics: Arc<Metrics>) -> Router {
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
        .nest(
            "/auth",
            protected_router(backends.auth.clone()).merge(accounts_router(accounts)),
        )
        .merge(workflows_routes)
        .merge(runs_router(runs_state))
        .merge(credentials_router(credentials_state))
        .merge(metrics_router(metrics.clone()));
    // Traçage en couche externe (toutes les requêtes, 404 compris), métriques
    // par route à l'intérieur.
    with_request_tracing(with_metrics(router, metrics))
}

/// État des endpoints de comptes : Argon2id sur les stores de `backends`,
/// anti-bruteforce par email et par adresse IP (#190).
fn accounts_state(
    backends: &Backends,
    ip_throttle: IpThrottle,
) -> autome_core::Result<AccountsState> {
    Ok(AccountsState::new(
        backends.auth.clone(),
        backends.users.clone(),
        Arc::new(Argon2PasswordHasher::new()),
    )?
    .with_throttle(backends.login_throttle.clone())
    .with_ip_throttle(ip_throttle))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let log_format = telemetry::init(config.profile)?;
    tracing::debug!(log_format = ?log_format, "logs initialisés");

    // Backend de persistance (dette #107) : `AUTOME_PERSISTENCE=postgres` adosse
    // les stores d'auth à PostgreSQL ; défaut `memory` (dev/tests/single-node).
    // Le secret JWT vient toujours de `config.jwt_secret` (constat 1).
    let mode = PersistenceMode::from_env()?;
    let queue_mode = QueueMode::from_env()?;
    // Garde prod (#172) : refus de démarrer en `prod` sur des backends in-memory
    // (l'état serait perdu et invisible du worker).
    ensure_durable_backends(config.profile, mode, queue_mode)?;

    // Runner de migrations (dette #107) : en mode `postgres` avec
    // `AUTOME_MIGRATE=1`, applique les migrations manquantes au démarrage
    // (idempotent, sérialisé entre répliques par un verrou applicatif).
    if mode == PersistenceMode::Postgres && migrate_on_boot_requested() {
        let applied = migrate(&config).await?;
        tracing::info!(applied, "migrations appliquées");
    }

    // File de messages (dette #107) : `AUTOME_QUEUE=rabbitmq` se connecte au
    // broker (`QUEUE_URL`) ; défaut `memory`. Le dispatcher de runs publie dessus.
    let queue = build_queue(&config, queue_mode).await?;
    let backends = build_backends(&config, mode, queue)?;
    tracing::info!(queue = ?queue_mode, persistence = ?mode, "backends prêts");

    // Maintenance (#182) : purge des tentatives de connexion et des
    // révocations expirées (sinon les tables croissent sans fin). Sûr sur
    // plusieurs répliques à la fois.
    let (throttle, revocations, clock) = (
        backends.login_throttle.clone(),
        backends.auth.revocation.clone(),
        backends.auth.clock.clone(),
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            tick.tick().await;
            let now = clock.now_unix_seconds();
            match purge_expired(throttle.as_ref(), revocations.as_ref(), now).await {
                Ok((attempts, revoked)) => {
                    tracing::debug!(
                        attempts,
                        revoked,
                        "entrées d'authentification expirées purgées"
                    )
                }
                Err(e) => {
                    tracing::warn!(error = %e, "purge des entrées d'authentification impossible")
                }
            }
        }
    });

    // Anti-bruteforce par IP (#190) : configuration invalide ⇒ refus de démarrer.
    let ip_throttle = IpThrottle::from_env(&config)?;
    tracing::info!(
        login_max = ip_throttle.login_max,
        signup_max = ip_throttle.signup_max,
        "limitation des tentatives par adresse IP"
    );
    let accounts = accounts_state(&backends, ip_throttle)?;
    let metrics = Arc::new(Metrics::new());
    let app = build_router(&backends, accounts, metrics);

    // F6.4 — HTTPS via rustls si `AUTOME_TLS_CERT` + `AUTOME_TLS_KEY`
    // sont positionnées ; sinon HTTP en clair. Décision all-or-nothing
    // (pas de fallback silencieux qui dégraderait la sécurité).
    let tls_cert = std::env::var("AUTOME_TLS_CERT").ok();
    let tls_key = std::env::var("AUTOME_TLS_KEY").ok();
    match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let tls_config = load_rustls_config(&cert, &key).await?;
            let addr: std::net::SocketAddr = config.http_addr.parse()?;
            tracing::info!(profile = ?config.profile, %addr, tls = true, "autome-api en écoute (https)");
            axum_server::bind_rustls(addr, tls_config)
                // Adresse du pair : IP cliente de l'anti-bruteforce (#190).
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await?;
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(&config.http_addr).await?;
            tracing::info!(
                profile = ?config.profile,
                addr = %config.http_addr,
                tls = false,
                "autome-api en écoute (http)"
            );
            // Adresse du pair : IP cliente de l'anti-bruteforce (#190).
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await?;
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
        let queue = std::sync::Arc::new(autome_core::InMemoryQueue::new());
        let backends =
            build_backends(&config, PersistenceMode::Memory, queue).expect("backends in-memory");
        let ip_throttle = IpThrottle::from_source(&config, |_| None).expect("défauts");
        let accounts = accounts_state(&backends, ip_throttle).expect("état comptes");
        let _app: Router = build_router(&backends, accounts, Arc::new(Metrics::new()));
    }
}

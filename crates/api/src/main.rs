//! `autome-api` — binaire du serveur HTTP (axum).
//!
//! Conformément à l'ADR 0001, axum est confiné à ce crate ; `core` et
//! `engine` n'en dépendent pas. Squelette F1.3.d : un routeur avec une
//! sonde de santé. Le CRUD workflows et le déclenchement de runs relèvent
//! du domaine 7 (F7.*). Le middleware d'authentification (F6.1.c) vit dans
//! [`autome_api::auth`].

use std::sync::Arc;

use autome_api::accounts::{accounts_router, AccountsState, IpThrottle};
use autome_api::admin::{admin_router, promote_admin, AdminState, Promotion};
use autome_api::auth::protected_router;
use autome_api::credentials::{credentials_router, CredentialsState};
use autome_api::mail::MailSettings;
use autome_api::metrics::{metrics_router, with_metrics, Metrics};
use autome_api::observability::with_request_tracing;
use autome_api::password_reset::{PasswordReset, ResetTokens};
use autome_api::probes::{probes_router, ProbesState};
use autome_api::runs::{runs_router, RunsState};
use autome_api::runtime::{
    build_backends, build_health_checks, build_queue, ensure_durable_backends, migrate,
    migrate_on_boot_requested, purge_expired, Backends, PersistenceMode, QueueMode,
};
use autome_api::shutdown::{
    begin_shutdown, serve_tls_until, serve_until, termination_signal, Draining, ShutdownConfig,
};
use autome_api::tls::load_rustls_config;
use autome_api::webhooks::{max_per_window_from_env, webhooks_router, WebhookKeys, WebhooksState};
use autome_api::workflows::{
    create_workflow, delete_workflow, get_workflow, list_workflows, update_workflow, WorkflowsState,
};
use autome_core::{telemetry, Argon2PasswordHasher, Config};
use autome_persistence::{connect, PgAuditSink, PgUserStore};
use axum::{routing::get, Router};

/// Période de la purge des entrées d'authentification expirées.
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// Construit le routeur racine : sondes (`/livez`, `/readyz`, `/health`) et
/// `/metrics` publics,
/// `/auth/signup|login|refresh` publics (émission des jetons), `/auth/whoami|admin`
/// protégés par JWT, l'administration des utilisateurs (`/admin/users`, #208),
/// le CRUD `workflows` + `runs` (domaine 7) et les webhooks
/// (`/hooks/{token}` public, gestion des URL protégée — #202). Toutes les
/// routes sont instrumentées par le middleware de métriques (F10.6.a).
///
/// Les routes workflows sont posées **à plat** (`/workflows`, `/workflows/:id`)
/// plutôt que `nest`ées : le catch-all d'un nest entrerait en conflit avec
/// `/workflows/:id/runs` du router runs.
///
/// Isolé de `main` pour être testable sans démarrer de serveur.
fn build_router(
    backends: &Backends,
    accounts: AccountsState,
    webhooks: WebhooksState,
    probes: ProbesState,
    metrics: Arc<Metrics>,
) -> Router {
    let wf_state = WorkflowsState::new(backends.auth.clone(), backends.workflows.clone())
        .with_webhooks(backends.webhooks.clone());
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
        .merge(probes_router(probes))
        .nest(
            "/auth",
            protected_router(backends.auth.clone()).merge(accounts_router(accounts)),
        )
        .merge(admin_router(AdminState::new(
            backends.auth.clone(),
            backends.users.clone(),
        )))
        .merge(workflows_routes)
        .merge(runs_router(runs_state))
        .merge(webhooks_router(webhooks))
        .merge(credentials_router(credentials_state))
        .merge(metrics_router(metrics.clone()));
    // Traçage en couche externe (toutes les requêtes, 404 compris), métriques
    // par route à l'intérieur.
    with_request_tracing(with_metrics(router, metrics))
}

/// État des webhooks (#202) : clé dérivée d'`ENCRYPTION_KEY`, quota par
/// endpoint sur le compteur partagé de l'anti-bruteforce.
fn webhooks_state(backends: &Backends, config: &Config, max_per_window: u32) -> WebhooksState {
    WebhooksState::new(
        backends.auth.clone(),
        backends.workflows.clone(),
        backends.webhooks.clone(),
        backends.dispatcher.clone(),
        backends.login_throttle.clone(),
        WebhookKeys::from_config(config),
    )
    .with_max_per_window(max_per_window)
}

/// État des endpoints de comptes : Argon2id sur les stores de `backends`,
/// anti-bruteforce par email et par adresse IP (#190), mot de passe oublié
/// si un envoi d'email est configuré (#204).
fn accounts_state(
    backends: &Backends,
    ip_throttle: IpThrottle,
    password_reset: Option<PasswordReset>,
) -> autome_core::Result<AccountsState> {
    let accounts = AccountsState::new(
        backends.auth.clone(),
        backends.users.clone(),
        Arc::new(Argon2PasswordHasher::new()),
    )?
    .with_throttle(backends.login_throttle.clone())
    .with_ip_throttle(ip_throttle);
    Ok(match password_reset {
        Some(reset) => accounts.with_password_reset(reset),
        None => accounts,
    })
}

/// Mot de passe oublié (#204) : `None` sans envoi d'email possible (`prod`
/// sans `AUTOME_SMTP_URL`). Configuration invalide ⇒ refus de démarrer.
fn password_reset(config: &Config) -> Result<Option<PasswordReset>, String> {
    let Some(mail) = MailSettings::from_env(config.profile)? else {
        tracing::warn!(
            "mot de passe oublié désactivé : AUTOME_SMTP_URL absent (503 sur /auth/password/forgot)"
        );
        return Ok(None);
    };
    tracing::info!(public_url = %mail.public_url, "mot de passe oublié activé");
    Ok(Some(PasswordReset::new(
        ResetTokens::from_config(config),
        mail,
    )))
}

/// `autome-api promote-admin <email>` (#208) : promeut un compte existant
/// super-admin dans la base `DATABASE_URL`, puis s'arrête. Seul moyen de créer
/// le premier administrateur : il exige l'accès à l'infrastructure (`kubectl
/// exec`, `docker compose exec`), jamais l'API publique.
async fn promote_admin_command(
    config: &Config,
    email: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if PersistenceMode::from_env()? != PersistenceMode::Postgres {
        return Err("promote-admin requiert AUTOME_PERSISTENCE=postgres (les comptes en mémoire ne survivent pas au processus)".into());
    }
    let pool = connect(&config.database_url)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    match promote_admin(
        &PgUserStore::new(pool.clone()),
        &PgAuditSink::new(pool),
        email,
        now_ms,
    )
    .await?
    {
        Promotion::Promoted {
            user_id,
            reactivated,
        } => {
            tracing::info!(%user_id, reactivated, "compte promu administrateur");
            Ok(())
        }
        Promotion::UnknownEmail => {
            Err(format!("aucun compte pour {email} : inscrivez-le d'abord").into())
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let log_format = telemetry::init(config.profile)?;
    tracing::debug!(log_format = ?log_format, "logs initialisés");

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {}
        Some("promote-admin") => {
            let [_, email] = args.as_slice() else {
                return Err("usage : autome-api promote-admin <email>".into());
            };
            return promote_admin_command(&config, email).await;
        }
        Some(other) => {
            return Err(
                format!("commande inconnue « {other} » (seule : promote-admin <email>)").into(),
            )
        }
    }

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
    // Sonde `/readyz` (#192) : base et broker effectivement sélectionnés ;
    // 503 `draining` dès le début de l'arrêt gracieux (#194).
    let draining = Draining::default();
    let probes = ProbesState::new(build_health_checks(
        &config,
        mode,
        queue_mode,
        queue.clone(),
    )?)
    .with_draining(draining.clone());
    let shutdown = ShutdownConfig::from_env()?;
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
        reset_max = ip_throttle.reset_max,
        "limitation des tentatives par adresse IP"
    );
    let accounts = accounts_state(&backends, ip_throttle, password_reset(&config)?)?;
    // Webhooks (#202) : quota invalide ⇒ refus de démarrer.
    let webhook_max = max_per_window_from_env()?;
    tracing::info!(
        max_per_window = webhook_max,
        "quota des webhooks par endpoint"
    );
    let webhooks = webhooks_state(&backends, &config, webhook_max);
    let metrics = Arc::new(Metrics::new());
    let app = build_router(&backends, accounts, webhooks, probes, metrics);

    // Arrêt gracieux (#194) : SIGTERM/SIGINT ⇒ readiness à 503, délai de
    // retrait, puis drain borné des requêtes en cours.
    tracing::info!(
        delay_s = shutdown.delay.as_secs(),
        timeout_s = shutdown.timeout.as_secs(),
        "arrêt gracieux configuré"
    );
    let stop = begin_shutdown(termination_signal(), draining, shutdown.delay);

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
            serve_tls_until(addr, tls_config, app, stop, shutdown.timeout).await?;
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(&config.http_addr).await?;
            tracing::info!(
                profile = ?config.profile,
                addr = %config.http_addr,
                tls = false,
                "autome-api en écoute (http)"
            );
            serve_until(listener, app, stop, shutdown.timeout).await?;
        }
        _ => {
            return Err(
                "TLS partiellement configuré : positionnez AUTOME_TLS_CERT *et* AUTOME_TLS_KEY"
                    .into(),
            );
        }
    }
    tracing::info!("autome-api arrêtée");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let reset = password_reset(&config).expect("journal en dev");
        assert!(reset.is_some(), "dev sans SMTP : liens journalisés");
        let accounts = accounts_state(&backends, ip_throttle, reset).expect("état comptes");
        let probes = ProbesState::new(vec![]);
        let webhooks = webhooks_state(&backends, &config, 10);
        let _app: Router = build_router(
            &backends,
            accounts,
            webhooks,
            probes,
            Arc::new(Metrics::new()),
        );
    }
}

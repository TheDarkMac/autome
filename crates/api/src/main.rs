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
use autome_api::cors::{allowed_origins_from_env, with_cors};
use autome_api::credentials::{credentials_router, CredentialsState};
use autome_api::email_verification::{
    verification_required_from_source, verify_email_by_operator, EmailVerification,
    OperatorVerification, VerifyTokens,
};
use autome_api::erasure::Erasure;
use autome_api::invitation::{
    invite_by_operator, signup_open_from_source, Invitation, InviteTokens, Invited,
};
use autome_api::mail::MailSettings;
use autome_api::metrics::{metrics_router, with_metrics, Metrics};
use autome_api::observability::with_request_tracing;
use autome_api::openapi::openapi_router;
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
use axum::http::HeaderValue;
use axum::{routing::get, Router};

/// Période de la purge des entrées d'authentification expirées.
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// Construit le routeur racine : sondes (`/livez`, `/readyz`, `/health`),
/// `/metrics` et la spec `/openapi.json` publics,
/// `/auth/signup|login|refresh` publics (émission des jetons), `/auth/whoami|admin`
/// protégés par JWT, l'administration des utilisateurs (`/admin/users`, #208),
/// le CRUD `workflows` + `runs` (domaine 7) et les webhooks
/// (`/hooks/{token}` public, gestion des URL protégée — #202). Toutes les
/// routes sont instrumentées par le middleware de métriques (F10.6.a) et
/// répondent au CORS des origines `cors` (#225).
///
/// Les routes workflows sont posées **à plat** (`/workflows`, `/workflows/:id`)
/// plutôt que `nest`ées : le catch-all d'un nest entrerait en conflit avec
/// `/workflows/:id/runs` du router runs.
///
/// Isolé de `main` pour être testable sans démarrer de serveur.
fn build_router(
    backends: &Backends,
    accounts: AccountsState,
    admin: AdminState,
    webhooks: WebhooksState,
    probes: ProbesState,
    metrics: Arc<Metrics>,
    cors: Vec<HeaderValue>,
) -> Router {
    let wf_state = WorkflowsState::new(backends.auth.clone(), backends.workflows.clone())
        .with_webhooks(backends.webhooks.clone())
        .with_schedules(backends.schedules.clone());
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
        .merge(openapi_router())
        .nest(
            "/auth",
            protected_router(backends.auth.clone()).merge(accounts_router(accounts)),
        )
        .merge(admin_router(admin))
        .merge(workflows_routes)
        .merge(runs_router(runs_state))
        .merge(webhooks_router(webhooks))
        .merge(credentials_router(credentials_state))
        .merge(metrics_router(metrics.clone()));
    // Traçage en couche externe (toutes les requêtes, 404 et préflights
    // compris), CORS ensuite, métriques par route à l'intérieur.
    with_request_tracing(with_cors(with_metrics(router, metrics), cors))
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

/// Suppression de compte (#229) : effaceur des backends, compteurs
/// anti-bruteforce partagés.
fn erasure(backends: &Backends) -> Erasure {
    Erasure::new(backends.eraser.clone(), backends.login_throttle.clone())
}

/// Fonctions qui envoient des emails : mot de passe oublié (#204),
/// vérification de l'adresse (#231) et invitations (#241).
#[derive(Debug, Default)]
struct MailFeatures {
    password_reset: Option<PasswordReset>,
    email_verification: Option<EmailVerification>,
    invitation: Option<Invitation>,
}

/// État de l'administration (#208) : suppression de comptes (#229) et
/// invitations si un envoi d'email est configuré (#241).
fn admin_state(backends: &Backends, invitation: Option<Invitation>) -> AdminState {
    let admin = AdminState::new(backends.auth.clone(), backends.users.clone())
        .with_erasure(erasure(backends));
    match invitation {
        Some(invitation) => admin.with_invitation(invitation),
        None => admin,
    }
}

/// État des endpoints de comptes : Argon2id sur les stores de `backends`,
/// anti-bruteforce par email et par adresse IP (#190), mot de passe oublié,
/// vérification de l'email et acceptation des invitations si un envoi d'email
/// est configuré (#204, #231, #241), inscriptions ouvertes ou non (#241).
fn accounts_state(
    backends: &Backends,
    ip_throttle: IpThrottle,
    mail: MailFeatures,
    signup_open: bool,
) -> autome_core::Result<AccountsState> {
    let accounts = AccountsState::new(
        backends.auth.clone(),
        backends.users.clone(),
        Arc::new(Argon2PasswordHasher::new()),
    )?
    .with_throttle(backends.login_throttle.clone())
    .with_ip_throttle(ip_throttle)
    .with_erasure(erasure(backends))
    .with_signup_open(signup_open);
    let accounts = match mail.invitation {
        Some(invitation) => accounts.with_invitations(invitation.tokens),
        None => accounts,
    };
    let accounts = match mail.password_reset {
        Some(reset) => accounts.with_password_reset(reset),
        None => accounts,
    };
    Ok(match mail.email_verification {
        Some(verification) => accounts.with_email_verification(verification),
        None => accounts,
    })
}

/// Fonctions d'email depuis l'environnement (`AUTOME_SMTP_URL`…,
/// `AUTOME_EMAIL_VERIFICATION`). Configuration invalide ⇒ refus de démarrer.
fn mail_features(config: &Config) -> Result<MailFeatures, String> {
    let required = verification_required_from_source(
        std::env::var("AUTOME_EMAIL_VERIFICATION").ok().as_deref(),
    )?;
    Ok(mail_features_from(
        config,
        MailSettings::from_env(config.profile)?,
        required,
    ))
}

/// Sans envoi d'email possible (`prod` sans SMTP), ni mot de passe oublié
/// ni invitations (503), ni vérification de l'email (comptes créés
/// vérifiés) ; la vérification peut aussi être désactivée (`required =
/// false`).
fn mail_features_from(config: &Config, mail: Option<MailSettings>, required: bool) -> MailFeatures {
    let Some(mail) = mail else {
        tracing::warn!(
            "mot de passe oublié, invitations et vérification de l'email désactivés : \
             AUTOME_SMTP_URL absent (503 sur /auth/password/forgot et /admin/users en POST, \
             comptes créés vérifiés)"
        );
        return MailFeatures::default();
    };
    tracing::info!(public_url = %mail.public_url, "mot de passe oublié activé");
    let email_verification = if required {
        tracing::info!("vérification de l'email exigée à l'inscription");
        Some(EmailVerification::new(
            VerifyTokens::from_config(config),
            mail.clone(),
        ))
    } else {
        tracing::warn!("vérification de l'email désactivée (AUTOME_EMAIL_VERIFICATION=off)");
        None
    };
    MailFeatures {
        invitation: Some(Invitation::new(
            InviteTokens::from_config(config),
            mail.clone(),
        )),
        password_reset: Some(PasswordReset::new(ResetTokens::from_config(config), mail)),
        email_verification,
    }
}

/// `AUTOME_SIGNUP` (#241) : inscriptions publiques ouvertes (`open`, défaut)
/// ou invitation seulement (`invite_only`). Valeur invalide ⇒ refus de
/// démarrer.
fn signup_open() -> Result<bool, String> {
    let open = signup_open_from_source(std::env::var("AUTOME_SIGNUP").ok().as_deref())?;
    if !open {
        tracing::info!("inscriptions publiques fermées : comptes créés par invitation");
    }
    Ok(open)
}

/// `autome-api invite <email> [role]` (#241) : crée un compte invité (rôle
/// `editor` par défaut) dans la base `DATABASE_URL` et lui envoie
/// l'invitation, puis s'arrête — amorçage d'une instance aux inscriptions
/// fermées (`invite <email> admin`). Exige l'accès à l'infrastructure et un
/// envoi d'email (SMTP, ou journal en `dev`).
async fn invite_command(
    config: &Config,
    email: &str,
    role: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if PersistenceMode::from_env()? != PersistenceMode::Postgres {
        return Err("invite requiert AUTOME_PERSISTENCE=postgres (les comptes en mémoire ne survivent pas au processus)".into());
    }
    let Some(mail) = MailSettings::from_env(config.profile)? else {
        return Err("invite requiert un envoi d'email (AUTOME_SMTP_URL)".into());
    };
    let invitation = Invitation::new(InviteTokens::from_config(config), mail);
    let pool = connect(&config.database_url)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    match invite_by_operator(
        &PgUserStore::new(pool.clone()),
        &PgAuditSink::new(pool),
        &invitation,
        email,
        role,
        now_ms,
    )
    .await?
    {
        Invited::Created { user, sent: true } => {
            tracing::info!(user_id = %user.id, "invitation envoyée");
            Ok(())
        }
        Invited::Created { user, sent: false } => Err(format!(
            "compte {} créé mais invitation non envoyée : la renvoyer depuis l'administration",
            user.id
        )
        .into()),
        Invited::EmailTaken => Err(format!("un compte existe déjà pour {email}").into()),
    }
}

/// `autome-api verify-email <email>` (#231) : marque vérifiée l'adresse d'un
/// compte existant dans la base `DATABASE_URL`, puis s'arrête (envoi d'emails
/// en panne, tests de bout en bout). Exige l'accès à l'infrastructure.
async fn verify_email_command(
    config: &Config,
    email: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if PersistenceMode::from_env()? != PersistenceMode::Postgres {
        return Err("verify-email requiert AUTOME_PERSISTENCE=postgres (les comptes en mémoire ne survivent pas au processus)".into());
    }
    let pool = connect(&config.database_url)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    match verify_email_by_operator(
        &PgUserStore::new(pool.clone()),
        &PgAuditSink::new(pool),
        email,
        now_ms,
    )
    .await?
    {
        OperatorVerification::Verified { user_id, already } => {
            tracing::info!(%user_id, already, "adresse email vérifiée");
            Ok(())
        }
        OperatorVerification::UnknownEmail => {
            Err(format!("aucun compte pour {email} : inscrivez-le d'abord").into())
        }
    }
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
        Some("verify-email") => {
            let [_, email] = args.as_slice() else {
                return Err("usage : autome-api verify-email <email>".into());
            };
            return verify_email_command(&config, email).await;
        }
        Some("invite") => {
            return match args.as_slice() {
                [_, email] => invite_command(&config, email, None).await,
                [_, email, role] => invite_command(&config, email, Some(role)).await,
                _ => Err("usage : autome-api invite <email> [admin|editor|viewer]".into()),
            };
        }
        Some(other) => {
            return Err(format!(
                "commande inconnue « {other} » (promote-admin <email>, verify-email <email>, \
                 invite <email> [role])"
            )
            .into())
        }
    }

    // Proxys sortants ignorés par le client filtré anti-SSRF (#233).
    let proxies = autome_connectors::proxy_vars_set(|k| std::env::var(k).ok());
    if !proxies.is_empty() {
        tracing::warn!(
            variables = ?proxies,
            "proxy sortant ignoré par les requêtes HTTP filtrées (OAuth) : la sortie se fait \
             en direct ; passer par une passerelle transparente si le réseau l'impose"
        );
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
        verify_max = ip_throttle.verify_max,
        "limitation des tentatives par adresse IP"
    );
    let mail = mail_features(&config)?;
    let admin = admin_state(&backends, mail.invitation.clone());
    let accounts = accounts_state(&backends, ip_throttle, mail, signup_open()?)?;
    // Webhooks (#202) : quota invalide ⇒ refus de démarrer.
    let webhook_max = max_per_window_from_env()?;
    tracing::info!(
        max_per_window = webhook_max,
        "quota des webhooks par endpoint"
    );
    let webhooks = webhooks_state(&backends, &config, webhook_max);
    let metrics = Arc::new(Metrics::new());
    // CORS (#225) : origine invalide ⇒ refus de démarrer.
    let cors = allowed_origins_from_env(config.profile)?;
    tracing::info!(
        origins = ?cors.iter().filter_map(|o| o.to_str().ok()).collect::<Vec<_>>(),
        "origines CORS autorisées"
    );
    let app = build_router(&backends, accounts, admin, webhooks, probes, metrics, cors);

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

    /// Routeur racine complet avec des backends in-memory dérivés d'une
    /// config de dev.
    fn dev_router() -> Router {
        let config = Config::load(|_| None).expect("config dev par défaut");
        let queue = std::sync::Arc::new(autome_core::InMemoryQueue::new());
        let backends =
            build_backends(&config, PersistenceMode::Memory, queue).expect("backends in-memory");
        let ip_throttle = IpThrottle::from_source(&config, |_| None).expect("défauts");
        let mail = MailSettings::from_source(config.profile, |_| None).expect("journal en dev");
        let features = mail_features_from(&config, mail, true);
        assert!(
            features.password_reset.is_some()
                && features.email_verification.is_some()
                && features.invitation.is_some(),
            "dev sans SMTP : liens journalisés"
        );
        let admin = admin_state(&backends, features.invitation.clone());
        let accounts =
            accounts_state(&backends, ip_throttle, features, true).expect("état comptes");
        let probes = ProbesState::new(vec![]);
        let webhooks = webhooks_state(&backends, &config, 10);
        build_router(
            &backends,
            accounts,
            admin,
            webhooks,
            probes,
            Arc::new(Metrics::new()),
            autome_api::cors::allowed_origins(config.profile, None).expect("défauts dev"),
        )
    }

    #[test]
    fn email_verification_follows_mail_and_setting() {
        let config = Config::load(|_| None).expect("config dev par défaut");
        let mail = || MailSettings::from_source(config.profile, |_| None).unwrap();

        // Sans envoi possible : rien (comptes créés vérifiés).
        let none = mail_features_from(&config, None, true);
        assert!(
            none.password_reset.is_none()
                && none.email_verification.is_none()
                && none.invitation.is_none()
        );
        // Désactivée explicitement : le mot de passe oublié reste disponible.
        let off = mail_features_from(&config, mail(), false);
        assert!(off.password_reset.is_some() && off.email_verification.is_none());
        let on = mail_features_from(&config, mail(), true);
        assert!(on.email_verification.is_some());
    }

    #[test]
    fn router_builds() {
        // Le routeur racine complet (health + auth + workflows + runs +
        // metrics + openapi) se construit sans panique ni conflit de routes.
        let _app: Router = dev_router();
    }

    #[tokio::test]
    async fn openapi_json_is_served_publicly_by_the_full_router() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        // #206 : `openapi_router` n'était pas monté ⇒ 404 sur un serveur lancé.
        // Sans Bearer : la spec est publique, comme les sondes.
        let response = dev_router()
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let spec: serde_json::Value =
            serde_json::from_slice(&bytes).expect("spec OpenAPI = JSON valide");
        assert!(spec["openapi"].as_str().unwrap_or("").starts_with("3."));
        assert!(spec["paths"]["/workflows"].is_object());
    }

    #[tokio::test]
    async fn the_dev_frontend_passes_the_cors_preflight_of_the_full_router() {
        use axum::body::Body;
        use axum::http::{header, Method, Request, StatusCode};
        use tower::ServiceExt;

        // #225 : sans CORS, le navigateur bloquait tous les appels du
        // frontend (préflight `OPTIONS` en 405).
        let response = dev_router()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/workflows/wf-1/runs")
                    .header(header::ORIGIN, "http://localhost:5173")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .header(
                        header::ACCESS_CONTROL_REQUEST_HEADERS,
                        "authorization,content-type",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://localhost:5173"
        );
        // Le traçage reste la couche externe : le préflight a son x-request-id.
        assert!(response.headers().contains_key("x-request-id"));
    }
}

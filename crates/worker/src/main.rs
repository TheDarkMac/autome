//! `autome-worker` — exécuteur de runs **de production** (dette #107, constat 2).
//!
//! Stateless (ARCHITECTURE.md §2) : consomme la file `runs`, charge l'état en
//! base, délègue l'exécution au moteur via [`RunExecutor`], persiste le
//! checkpoint final. File et stores sont sélectionnés par l'environnement
//! (mêmes sélecteurs que l'API) :
//!
//! - `AUTOME_QUEUE` : `memory` (défaut) | `rabbitmq` (`config.queue_url`) ;
//! - `AUTOME_PERSISTENCE` : `memory` (défaut) | `postgres` (`config.database_url`) ;
//! - `AUTOME_WORKER_CONCURRENCY` : nombre de tâches consommatrices (défaut 4).
//! - `AUTOME_RUN_TIMEOUT_SECS` : au-delà, un run toujours `running` est
//!   considéré interrompu et clos en `failed` (défaut 3600, plancher 60). Un
//!   run n'étant jamais rejoué (#174), c'est ce qui solde les runs d'un worker
//!   tué en cours d'exécution.
//! - `AUTOME_CONNECTOR_CREDENTIALS` : `per_user` (défaut) — chaque action de
//!   connecteur utilise le token OAuth **du propriétaire du run**, lu dans le
//!   store chiffré (`connector_tokens`, déchiffré avec `ENCRYPTION_KEY`, qui
//!   doit être celle de l'API) ; `single_tenant` — opt-in mono-tenant : repli
//!   sur le token global `AUTOME_CONNECTOR_TOKEN_<PROVIDER>` si le propriétaire
//!   n'a pas de token.
//! - `AUTOME_OAUTH_PROVIDERS` + `AUTOME_OAUTH_<ID>_*` : mêmes providers OAuth
//!   que l'API ; le worker s'en sert pour **rafraîchir** les access tokens
//!   expirés (refresh token stocké). Sans config, un token expiré fait échouer
//!   le node avec une invitation à reconnecter le compte.
//!
//! - `AUTOME_WORKER_PROBE_ADDR` : sondes `GET /livez` (consommateurs vivants,
//!   un run en cours comptant jusqu'à `AUTOME_RUN_TIMEOUT_SECS`) et
//!   `GET /readyz` (base et broker joignables) — défaut `127.0.0.1:9091`,
//!   `0.0.0.0:9091` dans l'image ; vide = désactivées (#192).
//!
//! - `AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS` : délai laissé aux runs en cours à
//!   l'arrêt (défaut 25 s, #200).
//!
//! - `AUTOME_SCHEDULER_TICK_SECS` : période de la boucle des déclencheurs
//!   `schedule` (cron / intervalle) et `polling` (défaut 10 s, `0` =
//!   désactivée, #227). Chaque réplique la fait tourner ; les échéances sont
//!   partagées en base (`trigger_schedules`) et une seule réplique déclenche
//!   chacune. Les polls passent par le client HTTP filtré anti-SSRF
//!   (`AUTOME_EGRESS_ALLOWLIST`). Sans PostgreSQL, rien n'est planifié.
//!
//! Arrêt **borné** : sur `SIGINT`/`SIGTERM`, le pool cesse de consommer,
//! `/readyz` passe à 503 et les runs en cours ont le délai d'arrêt pour se
//! terminer ; au-delà, ils sont clos en `failed` (« worker arrêté pendant
//! l'exécution ») au lieu de rester `running` jusqu'au balayage des orphelins.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use autome_connectors::oauth::{OAuthClient, OAuthRegistry};
use autome_connectors::{proxy_vars_set, EgressPolicy, ReqwestHttpClient};
use autome_core::{telemetry, Config, QueueRunDispatcher};
use autome_persistence::selectors::{
    build_connector_token_store, build_health_checks, build_queue, build_run_state_store,
    build_trigger_schedule_store, build_workflow_repository, ensure_durable_backends,
    PersistenceMode, QueueMode,
};
use autome_worker::probe::{self, ProbeState};
use autome_worker::shutdown::shutdown_timeout_from_env;
use autome_worker::triggers::tick_period_from_env;
use autome_worker::{
    drain, reap_orphans, ConnectorCredentialsMode, DrainOutcome, Handler, IdempotencyStore,
    InMemoryIdempotencyStore, RunExecutor, TriggerLoop, WorkerPool,
};

/// Queue des runs (alignée sur `QueueRunDispatcher::queue_name` par défaut).
const RUNS_QUEUE: &str = "runs";

/// Lit `AUTOME_WORKER_CONCURRENCY` (défaut 4, plancher 1).
fn concurrency_from_env() -> usize {
    std::env::var("AUTOME_WORKER_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(4)
        .max(1)
}

/// Période du balayage des runs orphelins.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Adresse du serveur de sondes par défaut (#192) — l'image Docker la remplace
/// par `0.0.0.0:9091` pour l'orchestrateur.
const DEFAULT_PROBE_ADDR: &str = "127.0.0.1:9091";

/// `AUTOME_WORKER_PROBE_ADDR` : adresse des sondes `/livez` et `/readyz` ;
/// vide = sondes désactivées.
fn probe_addr_from_env() -> Option<String> {
    let addr = std::env::var("AUTOME_WORKER_PROBE_ADDR")
        .unwrap_or_else(|_| DEFAULT_PROBE_ADDR.to_string());
    let addr = addr.trim();
    (!addr.is_empty()).then(|| addr.to_string())
}

/// Lit `AUTOME_RUN_TIMEOUT_SECS` en millisecondes (défaut 3600 s, plancher 60 s).
fn run_timeout_ms_from_env() -> i64 {
    let secs = std::env::var("AUTOME_RUN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(3_600)
        .max(60);
    secs.saturating_mul(1_000)
}

/// Horodatage Unix (ms) courant.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let log_format = telemetry::init(config.profile)?;
    let persistence = PersistenceMode::from_env()?;
    let queue_mode = QueueMode::from_env()?;
    // Garde prod (#172) : un worker in-memory ne verrait aucun run publié par l'API.
    ensure_durable_backends(config.profile, persistence, queue_mode)?;
    let concurrency = concurrency_from_env();
    let run_timeout_ms = run_timeout_ms_from_env();
    let credentials_mode = ConnectorCredentialsMode::from_env()?;
    // Proxys sortants ignorés par le client filtré anti-SSRF (#233).
    let proxies = proxy_vars_set(|k| std::env::var(k).ok());
    if !proxies.is_empty() {
        tracing::warn!(
            variables = ?proxies,
            "proxy sortant ignoré par les requêtes HTTP filtrées (http_request, polls, OAuth) : \
             la sortie se fait en direct ; passer par une passerelle transparente si le réseau l'impose"
        );
    }

    // Backends sélectionnés par l'environnement (partagés avec l'API).
    let queue = build_queue(&config, queue_mode).await?;
    // Sonde `/readyz` (#192) : base et broker effectivement sélectionnés.
    let health_checks = build_health_checks(&config, persistence, queue_mode, queue.clone())?;
    let repo = build_workflow_repository(&config, persistence)?;
    let store = build_run_state_store(&config, persistence)?;

    // Tokens des connecteurs : ceux du propriétaire de chaque run.
    let tokens = build_connector_token_store(&config, persistence)?;
    // Rafraîchissement des tokens expirés : client OAuth (registre depuis
    // l'env, transport filtré anti-SSRF comme côté API).
    let oauth = OAuthClient::new(
        OAuthRegistry::from_env()?,
        ReqwestHttpClient::with_policy(EgressPolicy::from_env()),
    );
    let resolver = credentials_mode.resolver(tokens, Arc::new(oauth));
    if credentials_mode == ConnectorCredentialsMode::SingleTenant {
        tracing::warn!(
            "AUTOME_CONNECTOR_CREDENTIALS=single_tenant : repli sur les tokens globaux \
             AUTOME_CONNECTOR_TOKEN_* (tous les utilisateurs agissent avec ces comptes)"
        );
    }

    // Balayage des orphelins (runs d'un worker tué) : sûr entre répliques,
    // `fail_if_running` ne touche jamais un run terminé.
    let reaper_store = Arc::clone(&store);
    let reaper = tokio::spawn(async move {
        let mut tick = tokio::time::interval(REAP_INTERVAL);
        loop {
            tick.tick().await;
            match reap_orphans(reaper_store.as_ref(), now_ms(), run_timeout_ms).await {
                Ok(0) => {}
                Ok(n) => tracing::warn!(runs = n, "runs interrompus clos en échec"),
                Err(e) => tracing::error!(error = %e, "balayage des runs orphelins impossible"),
            }
        }
    });

    // Déclencheurs planifiés (#227) : runs publiés sur la même file, comme
    // ceux de l'API.
    let scheduler_period = tick_period_from_env()?;
    let scheduler = match scheduler_period {
        Some(period) => {
            let dispatcher = QueueRunDispatcher::new(queue.clone(), Arc::clone(&store))
                .with_queue_name(RUNS_QUEUE);
            let triggers = TriggerLoop::new(
                build_trigger_schedule_store(&config, persistence)?,
                Arc::new(dispatcher),
                Arc::new(ReqwestHttpClient::with_policy(EgressPolicy::from_env())),
            );
            Some(triggers.spawn(period, now_ms))
        }
        None => {
            tracing::warn!("AUTOME_SCHEDULER_TICK_SECS=0 : déclencheurs planifiés désactivés");
            None
        }
    };

    let executor = RunExecutor::new(repo, Arc::clone(&store), resolver);
    // Runs en cours, pour clore ceux qu'un arrêt interromprait (#200).
    let in_flight = executor.in_flight();
    let handler: Arc<dyn Handler + Send + Sync> = Arc::new(executor);
    let idempotency: Arc<dyn IdempotencyStore + Send + Sync> =
        Arc::new(InMemoryIdempotencyStore::new());

    tracing::info!(
        profile = ?config.profile,
        queue = ?queue_mode,
        persistence = ?persistence,
        credentials = ?credentials_mode,
        concurrency,
        run_timeout_s = run_timeout_ms / 1_000,
        scheduler_tick_s = scheduler_period.map(|p| p.as_secs()),
        runs_queue = RUNS_QUEUE,
        log_format = ?log_format,
        "autome-worker démarré"
    );

    let pool = WorkerPool::new(queue, handler, idempotency, RUNS_QUEUE, concurrency);

    // Arrêt borné (#200) : délai laissé aux runs en cours ; `/readyz` à 503
    // dès le signal.
    let shutdown_timeout = shutdown_timeout_from_env()?;
    let draining = Arc::new(AtomicBool::new(false));

    // Sondes (#192) : adresse invalide ou déjà prise ⇒ refus de démarrer
    // (l'orchestrateur tuerait sinon un worker sain faute de réponse).
    let probes = match probe_addr_from_env() {
        Some(addr) => {
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .map_err(|e| format!("AUTOME_WORKER_PROBE_ADDR `{addr}` : {e}"))?;
            tracing::info!(%addr, "sondes /livez et /readyz en écoute");
            let state = ProbeState::new(pool.liveness(), health_checks, run_timeout_ms)
                .with_draining(Arc::clone(&draining));
            Some(tokio::spawn(probe::serve(listener, Arc::new(state))))
        }
        None => {
            tracing::warn!("AUTOME_WORKER_PROBE_ADDR vide : sondes désactivées");
            None
        }
    };

    let running = pool.start();

    // Arrêt gracieux : SIGINT (Ctrl-C) ou SIGTERM (orchestrateur).
    shutdown_signal().await;
    draining.store(true, Ordering::Release);
    // Plus aucun déclenchement planifié pendant le drain.
    if let Some(scheduler) = scheduler {
        scheduler.abort();
    }
    tracing::info!(
        timeout_s = shutdown_timeout.as_secs(),
        runs_en_cours = in_flight.snapshot().len(),
        "signal d'arrêt reçu : plus de nouveaux runs, drain des runs en cours"
    );
    let outcome = drain(running, &in_flight, store.as_ref(), shutdown_timeout).await;
    reaper.abort();
    if let Some(probes) = probes {
        probes.abort();
    }
    match outcome {
        DrainOutcome::Completed => {
            tracing::info!("autome-worker arrêté proprement");
            Ok(())
        }
        DrainOutcome::Interrupted { failed } => {
            tracing::warn!(
                runs = failed.len(),
                run_ids = ?failed,
                "délai d'arrêt dépassé : runs interrompus clos en échec"
            );
            // Le runtime attendrait sinon, à sa destruction, la fin des
            // threads bloquants du moteur — c'est-à-dire des runs qu'on vient
            // de clore.
            std::process::exit(0)
        }
    }
}

/// Attend `SIGINT` (Ctrl-C) ou, sous Unix, `SIGTERM`.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::warn!(error = %e, "SIGTERM indisponible"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

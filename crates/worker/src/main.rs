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
//! Arrêt **gracieux** : sur `SIGINT`/`SIGTERM`, le pool stoppe et `await` la
//! fin des tâches en cours (pas de message perdu en vol).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use autome_connectors::oauth::{OAuthClient, OAuthRegistry};
use autome_connectors::{EgressPolicy, ReqwestHttpClient};
use autome_core::Config;
use autome_persistence::selectors::{
    build_connector_token_store, build_queue, build_run_state_store, build_workflow_repository,
    ensure_durable_backends, PersistenceMode, QueueMode,
};
use autome_worker::{
    reap_orphans, ConnectorCredentialsMode, Handler, IdempotencyStore, InMemoryIdempotencyStore,
    RunExecutor, WorkerPool,
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
    let persistence = PersistenceMode::from_env()?;
    let queue_mode = QueueMode::from_env()?;
    // Garde prod (#172) : un worker in-memory ne verrait aucun run publié par l'API.
    ensure_durable_backends(config.profile, persistence, queue_mode)?;
    let concurrency = concurrency_from_env();
    let run_timeout_ms = run_timeout_ms_from_env();
    let credentials_mode = ConnectorCredentialsMode::from_env()?;

    // Backends sélectionnés par l'environnement (partagés avec l'API).
    let queue = build_queue(&config, queue_mode).await?;
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
        eprintln!(
            "autome-worker : AUTOME_CONNECTOR_CREDENTIALS=single_tenant — repli sur les tokens \
             globaux AUTOME_CONNECTOR_TOKEN_* (tous les utilisateurs agissent avec ces comptes)"
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
                Ok(n) => println!("autome-worker : {n} run(s) interrompu(s) clos en échec"),
                Err(e) => eprintln!("autome-worker : balayage des orphelins échoué : {e}"),
            }
        }
    });

    let handler: Arc<dyn Handler + Send + Sync> = Arc::new(RunExecutor::new(repo, store, resolver));
    let idempotency: Arc<dyn IdempotencyStore + Send + Sync> =
        Arc::new(InMemoryIdempotencyStore::new());

    println!(
        "autome-worker ({:?}) démarré — queue={:?} persistence={:?} credentials={:?} concurrency={} run_timeout={}s (file `{}`)",
        config.profile,
        queue_mode,
        persistence,
        credentials_mode,
        concurrency,
        run_timeout_ms / 1_000,
        RUNS_QUEUE
    );

    let pool = WorkerPool::new(queue, handler, idempotency, RUNS_QUEUE, concurrency);
    let running = pool.start();

    // Arrêt gracieux : SIGINT (Ctrl-C) ou SIGTERM (orchestrateur).
    shutdown_signal().await;
    println!("autome-worker : signal d'arrêt reçu — drain en cours…");
    running.stop().await;
    reaper.abort();
    println!("autome-worker : arrêté proprement.");
    Ok(())
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
            Err(e) => eprintln!("autome-worker : SIGTERM indisponible : {e}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

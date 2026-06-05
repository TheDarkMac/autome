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
//!
//! Arrêt **gracieux** : sur `SIGINT`/`SIGTERM`, le pool stoppe et `await` la
//! fin des tâches en cours (pas de message perdu en vol).

use std::sync::Arc;

use autome_core::Config;
use autome_persistence::selectors::{
    build_queue, build_run_state_store, build_workflow_repository, PersistenceMode, QueueMode,
};
use autome_worker::{Handler, IdempotencyStore, InMemoryIdempotencyStore, RunExecutor, WorkerPool};

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let persistence = PersistenceMode::from_env()?;
    let queue_mode = QueueMode::from_env()?;
    let concurrency = concurrency_from_env();

    // Backends sélectionnés par l'environnement (partagés avec l'API).
    let queue = build_queue(&config, queue_mode).await?;
    let repo = build_workflow_repository(&config, persistence)?;
    let store = build_run_state_store(&config, persistence)?;

    let handler: Arc<dyn Handler + Send + Sync> = Arc::new(RunExecutor::new(repo, store));
    let idempotency: Arc<dyn IdempotencyStore + Send + Sync> =
        Arc::new(InMemoryIdempotencyStore::new());

    println!(
        "autome-worker ({:?}) démarré — queue={:?} persistence={:?} concurrency={} (file `{}`)",
        config.profile, queue_mode, persistence, concurrency, RUNS_QUEUE
    );

    let pool = WorkerPool::new(queue, handler, idempotency, RUNS_QUEUE, concurrency);
    let running = pool.start();

    // Arrêt gracieux : SIGINT (Ctrl-C) ou SIGTERM (orchestrateur).
    shutdown_signal().await;
    println!("autome-worker : signal d'arrêt reçu — drain en cours…");
    running.stop().await;
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

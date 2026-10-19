//! Arrêt borné du worker (#200) et remise en file des runs interrompus (#239).
//!
//! Sur `SIGTERM` / `SIGINT`, le pool cesse de consommer et chaque run en cours
//! est prié de s'arrêter **au prochain nœud** ([`InFlightRuns::request_stop`]) :
//! son point de reprise est enregistré et il est remis en file, un autre
//! worker le reprendra sans rien rejouer. Un nœud long a
//! [`DEFAULT_SHUTDOWN_TIMEOUT`] (`AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS`) pour
//! se terminer. Au-delà — l'orchestrateur tuerait le conteneur à l'échéance de
//! sa période de grâce —, les runs encore dans un nœud sont **remis en file
//! tout de suite** ; le worker qui les reprend tranche : le nœud interrompu
//! n'est rejoué que s'il est rejouable (`autome_engine::rerunnable`), sinon le
//! run est clos en échec (livraison *au plus une fois*, #174). Un run sans
//! message de file (antérieur à l'outbox) est clos en échec
//! ([`INTERRUPTED_REASON`]) comme avant.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autome_core::{Queue, RunStateStore, RunStatus};

use crate::liveness::now_ms;
use crate::outbox::publish;
use crate::RunningPool;

/// Délai de drain par défaut, sous la période de grâce par défaut de k8s
/// (30 s) pour laisser le temps de remettre en file les runs interrompus.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(25);

/// Motif d'échec d'un run interrompu par l'arrêt du worker et qui ne peut
/// pas être remis en file (pas de message dans l'outbox).
pub const INTERRUPTED_REASON: &str =
    "worker arrêté pendant l'exécution : run interrompu (non rejoué, à relancer)";

/// `AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS` (secondes, défaut 25).
pub fn shutdown_timeout_from_env() -> Result<Duration, String> {
    shutdown_timeout_from(std::env::var("AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS").ok())
}

fn shutdown_timeout_from(value: Option<String>) -> Result<Duration, String> {
    match value {
        None => Ok(DEFAULT_SHUTDOWN_TIMEOUT),
        Some(v) if v.trim().is_empty() => Ok(DEFAULT_SHUTDOWN_TIMEOUT),
        Some(v) => v.trim().parse::<u64>().map(Duration::from_secs).map_err(|_| {
            format!("AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS : nombre de secondes attendu, obtenu `{v}`")
        }),
    }
}

/// Runs en cours d'exécution dans ce process (réclamés, pas encore terminés),
/// avec leur tentative, et demande d'arrêt partagée avec l'executor.
#[derive(Debug, Default)]
pub struct InFlightRuns {
    runs: Mutex<HashMap<String, i32>>,
    stop: AtomicBool,
}

impl InFlightRuns {
    /// Enregistre la tentative `attempt` de `run_id` jusqu'à la fin de vie du
    /// garde renvoyé (y compris si la tâche est interrompue).
    pub fn track(self: &Arc<Self>, run_id: &str, attempt: i32) -> InFlightGuard {
        self.lock().insert(run_id.to_string(), attempt);
        InFlightGuard {
            runs: Arc::clone(self),
            run_id: run_id.to_string(),
        }
    }

    /// Nombre de runs en cours.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// `true` si aucun run n'est en cours.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Runs en cours, triés.
    pub fn snapshot(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Runs en cours et leur tentative, triés par run.
    pub fn entries(&self) -> Vec<(String, i32)> {
        let mut entries: Vec<(String, i32)> =
            self.lock().iter().map(|(k, v)| (k.clone(), *v)).collect();
        entries.sort();
        entries
    }

    /// Demande aux runs en cours de s'arrêter au prochain nœud (arrêt du
    /// worker) ; définitif.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// L'arrêt a-t-il été demandé ?
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, i32>> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Retire le run des runs en cours à sa destruction.
#[derive(Debug)]
pub struct InFlightGuard {
    runs: Arc<InFlightRuns>,
    run_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.runs.lock().remove(&self.run_id);
    }
}

/// Issue du drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Tous les consommateurs ont terminé dans le délai (les runs arrêtés au
    /// prochain nœud ont été remis en file par l'executor).
    Completed,
    /// Délai dépassé : consommateurs interrompus en plein nœud.
    Interrupted {
        /// Runs remis en file (repris par un autre worker).
        requeued: Vec<String>,
        /// Runs clos en échec faute de pouvoir être remis en file.
        failed: Vec<String>,
    },
}

/// Arrête `running` : plus de nouveaux messages, arrêt des runs au prochain
/// nœud, puis au plus `timeout` pour que les consommateurs rendent la main.
/// Au-delà, les tâches sont interrompues et les runs encore en cours
/// ([`InFlightRuns`]) remis en file (message republié sur `queue`), ou clos
/// en échec s'ils n'ont pas de message.
pub async fn drain(
    mut running: RunningPool,
    in_flight: &InFlightRuns,
    store: &dyn RunStateStore,
    queue: &dyn Queue,
    timeout: Duration,
) -> DrainOutcome {
    in_flight.request_stop();
    if running.stop_within(timeout).await {
        return DrainOutcome::Completed;
    }
    // Relevé avant l'interruption : les gardes disparaissent avec les tâches.
    let interrupted = in_flight.entries();
    running.abort();
    let mut requeued = Vec::new();
    let mut failed = Vec::new();
    for (run_id, attempt) in interrupted {
        match store.requeue(&run_id, attempt, now_ms()).await {
            Ok(Some(message)) => {
                publish(queue, store, &message, now_ms()).await;
                requeued.push(run_id);
            }
            Ok(None) => {
                if close_without_message(store, &run_id, attempt).await {
                    failed.push(run_id);
                }
            }
            Err(e) => tracing::error!(
                run_id = %run_id,
                error = %e,
                "run interrompu non remis en file (le worker suivant le reprendra \
                 quand son battement aura expiré)"
            ),
        }
    }
    DrainOutcome::Interrupted { requeued, failed }
}

/// Clôt en échec un run interrompu que [`RunStateStore::requeue`] a refusé,
/// **seulement** s'il est toujours à cette tentative (pas de message dans
/// l'outbox) : un run déjà repris ailleurs ou terminé n'est pas touché.
async fn close_without_message(store: &dyn RunStateStore, run_id: &str, attempt: i32) -> bool {
    let still_ours = matches!(
        store.load(run_id).await,
        Ok(Some(cp)) if cp.status == RunStatus::Running && cp.attempt == attempt
    );
    if !still_ours {
        return false;
    }
    match store
        .fail_if_running(run_id, INTERRUPTED_REASON, now_ms())
        .await
    {
        Ok(closed) => closed,
        Err(e) => {
            tracing::error!(
                run_id = %run_id,
                error = %e,
                "run interrompu non clos (le balayage des orphelins s'en chargera)"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_track_runs_until_dropped() {
        let runs = Arc::new(InFlightRuns::default());
        let a = runs.track("r-b", 1);
        let b = runs.track("r-a", 3);
        assert_eq!(runs.snapshot(), vec!["r-a", "r-b"]);
        assert_eq!(
            runs.entries(),
            vec![("r-a".to_string(), 3), ("r-b".to_string(), 1)]
        );
        assert!(!runs.stop_requested());
        runs.request_stop();
        assert!(runs.stop_requested());
        drop(a);
        assert_eq!(runs.snapshot(), vec!["r-a"]);
        drop(b);
        assert!(runs.snapshot().is_empty());
    }

    #[test]
    fn timeout_configuration() {
        assert_eq!(
            shutdown_timeout_from(None).unwrap(),
            DEFAULT_SHUTDOWN_TIMEOUT
        );
        assert_eq!(
            shutdown_timeout_from(Some(" ".into())).unwrap(),
            DEFAULT_SHUTDOWN_TIMEOUT
        );
        assert_eq!(
            shutdown_timeout_from(Some("120".into())).unwrap(),
            Duration::from_secs(120)
        );
        let err = shutdown_timeout_from(Some("-1".into())).unwrap_err();
        assert!(err.contains("AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS"), "{err}");
    }
}

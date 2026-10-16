//! Arrêt borné du worker (#200).
//!
//! Sur `SIGTERM` / `SIGINT`, le pool cesse de consommer et les runs en cours
//! ont [`DEFAULT_SHUTDOWN_TIMEOUT`] (`AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS`) pour
//! se terminer normalement. Au-delà — l'orchestrateur tuerait le conteneur
//! à l'échéance de sa période de grâce —, les runs encore en cours sont **clos
//! tout de suite en `failed`** ([`INTERRUPTED_REASON`]) plutôt que de rester
//! `running` jusqu'au balayage des orphelins (jusqu'à une heure). Pas de
//! rejeu : des nodes ont pu produire leurs effets (livraison *au plus une
//! fois*, #174) ; le message non acquitté, redélivré, est acquitté sans
//! exécution puisque le run n'est plus `queued`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autome_core::RunStateStore;

use crate::liveness::now_ms;
use crate::RunningPool;

/// Délai de drain par défaut, sous la période de grâce par défaut de k8s
/// (30 s) pour laisser le temps de clore les runs interrompus.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(25);

/// Motif d'échec d'un run interrompu par l'arrêt du worker.
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

/// Runs en cours d'exécution dans ce process (réclamés, pas encore terminés).
#[derive(Debug, Default)]
pub struct InFlightRuns(Mutex<HashSet<String>>);

impl InFlightRuns {
    /// Enregistre `run_id` jusqu'à la fin de vie du garde renvoyé (y compris
    /// si la tâche est interrompue).
    pub fn track(self: &Arc<Self>, run_id: &str) -> InFlightGuard {
        self.lock().insert(run_id.to_string());
        InFlightGuard {
            runs: Arc::clone(self),
            run_id: run_id.to_string(),
        }
    }

    /// Runs en cours, triés.
    pub fn snapshot(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().iter().cloned().collect();
        ids.sort();
        ids
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.0
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
    /// Tous les consommateurs ont terminé dans le délai.
    Completed,
    /// Délai dépassé : consommateurs interrompus, ces runs clos en échec.
    Interrupted {
        /// Runs passés de `running` à `failed`.
        failed: Vec<String>,
    },
}

/// Arrête `running` : plus de nouveaux messages, puis au plus `timeout` pour
/// finir les runs en cours. Au-delà, les tâches sont interrompues et les runs
/// encore en cours ([`InFlightRuns`]) clos en échec dans `store`.
pub async fn drain(
    mut running: RunningPool,
    in_flight: &InFlightRuns,
    store: &dyn RunStateStore,
    timeout: Duration,
) -> DrainOutcome {
    if running.stop_within(timeout).await {
        return DrainOutcome::Completed;
    }
    // Relevé avant l'interruption : les gardes disparaissent avec les tâches.
    let interrupted = in_flight.snapshot();
    running.abort();
    let mut failed = Vec::with_capacity(interrupted.len());
    for run_id in interrupted {
        match store
            .fail_if_running(&run_id, INTERRUPTED_REASON, now_ms())
            .await
        {
            Ok(true) => failed.push(run_id),
            // Terminé entre-temps : rien à clore.
            Ok(false) => {}
            Err(e) => tracing::error!(
                run_id = %run_id,
                error = %e,
                "run interrompu non clos (le balayage des orphelins s'en chargera)"
            ),
        }
    }
    DrainOutcome::Interrupted { failed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_track_runs_until_dropped() {
        let runs = Arc::new(InFlightRuns::default());
        let a = runs.track("r-b");
        let b = runs.track("r-a");
        assert_eq!(runs.snapshot(), vec!["r-a", "r-b"]);
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

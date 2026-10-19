//! Métriques Prometheus du worker (#235), servies en `GET /metrics` par le
//! serveur de sondes ([`crate::probe`], port 9091).
//!
//! Registre **sans état global**, comme celui de l'API (F10.6.a) : un
//! [`WorkerMetrics`] partagé via `Arc` entre le pool, l'executor, la boucle
//! des déclencheurs et le balayage des orphelins.
//!
//! Tous les labels sont des **énumérations fixes** : aucun id de run, de
//! workflow ni de propriétaire n'apparaît dans les séries (cardinalité
//! bornée, rien d'identifiant sur un port joignable dans le réseau interne).
//!
//! | Série | Type | Labels |
//! |---|---|---|
//! | `autome_worker_deliveries_total` | counter | `outcome` : `ack`, `retry`, `dead_letter`, `deduplicated` |
//! | `autome_worker_queue_errors_total` | counter | — |
//! | `autome_worker_runs_total` | counter | `status` : `succeeded`, `failed` |
//! | `autome_worker_run_duration_seconds` | histogram | `status` |
//! | `autome_worker_runs_in_flight` | gauge | — |
//! | `autome_worker_orphan_runs_reaped_total` | counter | — |
//! | `autome_worker_trigger_runs_total` | counter | `kind` : `schedule`, `polling` ; `result` : `dispatched`, `failed` |
//! | `autome_worker_trigger_missed_total` | counter | — |
//! | `autome_worker_trigger_poll_errors_total` | counter | — |
//! | `autome_worker_trigger_tick_errors_total` | counter | — |
//! | `autome_worker_draining` | gauge | — |

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use autome_core::prometheus::{write_header, Histogram};

use crate::HandlerVerdict;

/// Bornes (secondes) du histogram de durée des runs : de la requête HTTP
/// unique au run d'une heure (`AUTOME_RUN_TIMEOUT_SECS` par défaut).
pub const RUN_DURATION_BUCKETS: &[f64] = &[
    0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0, 1800.0, 3600.0,
];

/// Sort d'une livraison prise dans la file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeliveryOutcome {
    /// Acquittée (run exécuté, ou livraison dupliquée d'un run déjà pris).
    Ack,
    /// Remise en file (échec transitoire).
    Retry,
    /// Envoyée en DLQ (run en échec, message invalide…).
    DeadLetter,
    /// Ignorée : clé d'idempotence déjà traitée par ce process.
    Deduplicated,
}

impl DeliveryOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Ack => "ack",
            Self::Retry => "retry",
            Self::DeadLetter => "dead_letter",
            Self::Deduplicated => "deduplicated",
        }
    }
}

impl From<&HandlerVerdict> for DeliveryOutcome {
    fn from(verdict: &HandlerVerdict) -> Self {
        match verdict {
            HandlerVerdict::Ok => Self::Ack,
            HandlerVerdict::Retry => Self::Retry,
            HandlerVerdict::DeadLetter => Self::DeadLetter,
        }
    }
}

/// Statut final d'un run exécuté par ce worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunOutcome {
    /// Terminé avec succès.
    Succeeded,
    /// Terminé en échec (node en erreur ou panic du moteur).
    Failed,
}

impl RunOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Type de déclencheur planifié.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TriggerKind {
    /// Cron / intervalle.
    Schedule,
    /// Polling d'une URL.
    Polling,
}

impl TriggerKind {
    /// Type d'un nœud d'après son `trigger_type` (seuls `schedule` et
    /// `polling` sont planifiés).
    pub fn of(trigger_type: &str) -> Self {
        if trigger_type == "polling" {
            Self::Polling
        } else {
            Self::Schedule
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Polling => "polling",
        }
    }
}

/// Valeurs lues au moment du rendu (état courant, pas des compteurs).
#[derive(Debug, Clone, Copy, Default)]
pub struct Gauges {
    /// Runs réclamés et pas encore terminés.
    pub runs_in_flight: usize,
    /// Arrêt en cours (#200).
    pub draining: bool,
}

#[derive(Default)]
struct Inner {
    deliveries: BTreeMap<DeliveryOutcome, u64>,
    queue_errors: u64,
    runs: BTreeMap<RunOutcome, Histogram>,
    orphans_reaped: u64,
    trigger_runs: BTreeMap<(TriggerKind, bool), u64>,
    trigger_missed: u64,
    trigger_poll_errors: u64,
    trigger_tick_errors: u64,
}

/// Registre des métriques du worker, partageable (`Arc<WorkerMetrics>`).
#[derive(Default)]
pub struct WorkerMetrics {
    inner: Mutex<Inner>,
}

impl WorkerMetrics {
    /// Registre vide.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Une livraison a reçu son sort.
    pub fn delivery(&self, outcome: DeliveryOutcome) {
        *self.lock().deliveries.entry(outcome).or_insert(0) += 1;
    }

    /// Erreur de la file (poll, ack ou nack impossible).
    pub fn queue_error(&self) {
        self.lock().queue_errors += 1;
    }

    /// Un run réclamé par ce worker s'est terminé après `elapsed`.
    pub fn run_finished(&self, outcome: RunOutcome, elapsed: Duration) {
        self.lock()
            .runs
            .entry(outcome)
            .or_insert_with(|| Histogram::new(RUN_DURATION_BUCKETS))
            .observe(elapsed.as_secs_f64());
    }

    /// `n` runs orphelins clos en échec par le balayage.
    pub fn orphans_reaped(&self, n: u64) {
        self.lock().orphans_reaped += n;
    }

    /// Run déclenché par la planification (`dispatched`), ou perdu faute
    /// d'avoir pu être dispatché.
    pub fn trigger_run(&self, kind: TriggerKind, dispatched: bool) {
        *self
            .lock()
            .trigger_runs
            .entry((kind, dispatched))
            .or_insert(0) += 1;
    }

    /// Échéance `schedule` sautée (retard).
    pub fn trigger_missed(&self) {
        self.lock().trigger_missed += 1;
    }

    /// Poll en échec (réseau, statut HTTP, réponse illisible).
    pub fn trigger_poll_error(&self) {
        self.lock().trigger_poll_errors += 1;
    }

    /// Passe de planification impossible (échéances illisibles).
    pub fn trigger_tick_error(&self) {
        self.lock().trigger_tick_errors += 1;
    }

    /// Rend les métriques au format d'exposition texte Prometheus (v0.0.4).
    /// Chaque série étiquetée est présente dès le départ (à zéro) pour que
    /// `rate()` et les alertes voient le premier événement.
    pub fn render(&self, gauges: Gauges) -> String {
        let inner = self.lock();
        let mut out = String::new();

        write_header(
            &mut out,
            "autome_worker_deliveries_total",
            "counter",
            "Queue deliveries handled, by outcome.",
        );
        for outcome in [
            DeliveryOutcome::Ack,
            DeliveryOutcome::Retry,
            DeliveryOutcome::DeadLetter,
            DeliveryOutcome::Deduplicated,
        ] {
            let n = inner.deliveries.get(&outcome).copied().unwrap_or(0);
            let _ = writeln!(
                out,
                "autome_worker_deliveries_total{{outcome=\"{}\"}} {n}",
                outcome.label()
            );
        }

        counter(
            &mut out,
            "autome_worker_queue_errors_total",
            "Queue errors (poll, ack, nack).",
            inner.queue_errors,
        );

        write_header(
            &mut out,
            "autome_worker_runs_total",
            "counter",
            "Runs executed by this worker, by final status.",
        );
        for outcome in [RunOutcome::Succeeded, RunOutcome::Failed] {
            let n = inner.runs.get(&outcome).map_or(0, Histogram::count);
            let _ = writeln!(
                out,
                "autome_worker_runs_total{{status=\"{}\"}} {n}",
                outcome.label()
            );
        }

        write_header(
            &mut out,
            "autome_worker_run_duration_seconds",
            "histogram",
            "Run duration from claim to final status, in seconds.",
        );
        for outcome in [RunOutcome::Succeeded, RunOutcome::Failed] {
            let empty = Histogram::new(RUN_DURATION_BUCKETS);
            let hist = inner.runs.get(&outcome).unwrap_or(&empty);
            let labels = format!("status=\"{}\"", outcome.label());
            hist.write(&mut out, "autome_worker_run_duration_seconds", &labels);
        }

        gauge(
            &mut out,
            "autome_worker_runs_in_flight",
            "Runs claimed by this worker and not finished yet.",
            gauges.runs_in_flight as u64,
        );
        counter(
            &mut out,
            "autome_worker_orphan_runs_reaped_total",
            "Interrupted runs closed as failed by the orphan sweep.",
            inner.orphans_reaped,
        );

        write_header(
            &mut out,
            "autome_worker_trigger_runs_total",
            "counter",
            "Runs fired by scheduled and polling triggers, by result.",
        );
        for kind in [TriggerKind::Schedule, TriggerKind::Polling] {
            for (dispatched, result) in [(true, "dispatched"), (false, "failed")] {
                let n = inner
                    .trigger_runs
                    .get(&(kind, dispatched))
                    .copied()
                    .unwrap_or(0);
                let _ = writeln!(
                    out,
                    "autome_worker_trigger_runs_total{{kind=\"{}\",result=\"{result}\"}} {n}",
                    kind.label()
                );
            }
        }
        counter(
            &mut out,
            "autome_worker_trigger_missed_total",
            "Scheduled occurrences skipped because they were too late.",
            inner.trigger_missed,
        );
        counter(
            &mut out,
            "autome_worker_trigger_poll_errors_total",
            "Failed polls (network, HTTP status, unreadable response).",
            inner.trigger_poll_errors,
        );
        counter(
            &mut out,
            "autome_worker_trigger_tick_errors_total",
            "Scheduler passes that could not read the due triggers.",
            inner.trigger_tick_errors,
        );

        gauge(
            &mut out,
            "autome_worker_draining",
            "1 while the worker is shutting down.",
            u64::from(gauges.draining),
        );
        out
    }
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    write_header(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    write_header(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_exposes_every_series_at_zero() {
        let out = WorkerMetrics::new().render(Gauges::default());
        for line in [
            "autome_worker_deliveries_total{outcome=\"ack\"} 0",
            "autome_worker_deliveries_total{outcome=\"deduplicated\"} 0",
            "autome_worker_queue_errors_total 0",
            "autome_worker_runs_total{status=\"failed\"} 0",
            "autome_worker_run_duration_seconds_count{status=\"succeeded\"} 0",
            "autome_worker_runs_in_flight 0",
            "autome_worker_orphan_runs_reaped_total 0",
            "autome_worker_trigger_runs_total{kind=\"polling\",result=\"failed\"} 0",
            "autome_worker_trigger_missed_total 0",
            "autome_worker_draining 0",
        ] {
            assert!(out.contains(&format!("{line}\n")), "{line} absent :\n{out}");
        }
        assert!(out.contains("# TYPE autome_worker_run_duration_seconds histogram\n"));
    }

    #[test]
    fn events_are_counted_per_label() {
        let m = WorkerMetrics::new();
        m.delivery(DeliveryOutcome::Ack);
        m.delivery(DeliveryOutcome::Ack);
        m.delivery(DeliveryOutcome::from(&HandlerVerdict::DeadLetter));
        m.queue_error();
        m.run_finished(RunOutcome::Succeeded, Duration::from_millis(200));
        m.run_finished(RunOutcome::Succeeded, Duration::from_secs(40));
        m.run_finished(RunOutcome::Failed, Duration::from_secs(2));
        m.orphans_reaped(3);
        m.trigger_run(TriggerKind::of("schedule"), true);
        m.trigger_run(TriggerKind::of("polling"), false);
        m.trigger_missed();
        m.trigger_poll_error();
        m.trigger_tick_error();
        let out = m.render(Gauges {
            runs_in_flight: 2,
            draining: true,
        });
        for line in [
            "autome_worker_deliveries_total{outcome=\"ack\"} 2",
            "autome_worker_deliveries_total{outcome=\"dead_letter\"} 1",
            "autome_worker_deliveries_total{outcome=\"retry\"} 0",
            "autome_worker_queue_errors_total 1",
            "autome_worker_runs_total{status=\"succeeded\"} 2",
            "autome_worker_runs_total{status=\"failed\"} 1",
            "autome_worker_run_duration_seconds_bucket{status=\"succeeded\",le=\"0.5\"} 1",
            "autome_worker_run_duration_seconds_bucket{status=\"succeeded\",le=\"30\"} 1",
            "autome_worker_run_duration_seconds_bucket{status=\"succeeded\",le=\"60\"} 2",
            "autome_worker_run_duration_seconds_sum{status=\"succeeded\"} 40.2",
            "autome_worker_run_duration_seconds_count{status=\"failed\"} 1",
            "autome_worker_runs_in_flight 2",
            "autome_worker_orphan_runs_reaped_total 3",
            "autome_worker_trigger_runs_total{kind=\"schedule\",result=\"dispatched\"} 1",
            "autome_worker_trigger_runs_total{kind=\"polling\",result=\"failed\"} 1",
            "autome_worker_trigger_runs_total{kind=\"polling\",result=\"dispatched\"} 0",
            "autome_worker_trigger_missed_total 1",
            "autome_worker_trigger_poll_errors_total 1",
            "autome_worker_trigger_tick_errors_total 1",
            "autome_worker_draining 1",
        ] {
            assert!(out.contains(&format!("{line}\n")), "{line} absent :\n{out}");
        }
    }

    #[test]
    fn every_sample_line_is_well_formed() {
        let m = WorkerMetrics::new();
        m.run_finished(RunOutcome::Failed, Duration::from_secs(1));
        let out = m.render(Gauges::default());
        for line in out.lines().filter(|l| !l.starts_with('#')) {
            let (series, value) = line.rsplit_once(' ').expect(line);
            assert!(series.starts_with("autome_worker_"), "{line}");
            assert!(value.parse::<f64>().is_ok(), "{line}");
        }
    }
}

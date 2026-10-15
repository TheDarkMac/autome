//! Liveness du worker (#192) : chaque consommateur bat au rythme de sa boucle.
//!
//! Un consommateur est **vivant** s'il a tourné récemment (poll, attente,
//! pause après erreur de file), **ou** s'il traite un message depuis moins que
//! le délai maximal d'un run : un run légitime de plusieurs minutes ne doit pas
//! faire redémarrer le pod. Un consommateur figé au-delà (poll bloqué, moteur
//! qui ne rend jamais la main) rend le worker « mort » : l'orchestrateur le
//! redémarre et le balayage des orphelins (#174) clôt ses runs.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Battement d'un consommateur.
#[derive(Debug)]
struct Beat {
    /// Dernier tour de boucle (ms Unix).
    last_tick_ms: AtomicI64,
    /// Début du traitement en cours (ms Unix), 0 si au repos.
    busy_since_ms: AtomicI64,
}

/// Battements de tous les consommateurs d'un pool.
#[derive(Debug)]
pub struct PoolLiveness {
    beats: Vec<Beat>,
}

/// Horodatage Unix (ms) courant.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl PoolLiveness {
    /// `consumers` consommateurs, considérés comme venant de battre à `now`.
    pub fn new(consumers: usize, now: i64) -> Self {
        Self {
            beats: (0..consumers)
                .map(|_| Beat {
                    last_tick_ms: AtomicI64::new(now),
                    busy_since_ms: AtomicI64::new(0),
                })
                .collect(),
        }
    }

    /// Tour de boucle du consommateur `id`.
    pub fn tick(&self, id: usize, now: i64) {
        if let Some(beat) = self.beats.get(id) {
            beat.last_tick_ms.store(now, Ordering::Release);
        }
    }

    /// Le consommateur `id` commence à traiter un message.
    pub fn busy(&self, id: usize, now: i64) {
        if let Some(beat) = self.beats.get(id) {
            beat.busy_since_ms.store(now.max(1), Ordering::Release);
        }
    }

    /// Le consommateur `id` a fini de traiter son message.
    pub fn idle(&self, id: usize, now: i64) {
        if let Some(beat) = self.beats.get(id) {
            beat.busy_since_ms.store(0, Ordering::Release);
            beat.last_tick_ms.store(now, Ordering::Release);
        }
    }

    /// `Ok` si tous les consommateurs sont vivants à `now` : dernier tour
    /// depuis moins de `stall_ms`, ou traitement en cours depuis moins de
    /// `busy_limit_ms`. Sinon, la liste des consommateurs figés.
    pub fn check(&self, now: i64, stall_ms: i64, busy_limit_ms: i64) -> Result<(), String> {
        let stuck: Vec<String> = self
            .beats
            .iter()
            .enumerate()
            .filter_map(|(id, beat)| {
                let busy_since = beat.busy_since_ms.load(Ordering::Acquire);
                if busy_since > 0 {
                    let busy = now - busy_since;
                    (busy >= busy_limit_ms)
                        .then(|| format!("consommateur {id} occupé depuis {busy} ms"))
                } else {
                    let quiet = now - beat.last_tick_ms.load(Ordering::Acquire);
                    (quiet >= stall_ms).then(|| format!("consommateur {id} figé depuis {quiet} ms"))
                }
            })
            .collect();
        if stuck.is_empty() {
            Ok(())
        } else {
            Err(stuck.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000;
    const STALL: i64 = 60_000;
    const BUSY: i64 = 3_660_000;

    #[test]
    fn fresh_and_ticking_consumers_are_alive() {
        let l = PoolLiveness::new(2, T0);
        assert!(l.check(T0 + STALL - 1, STALL, BUSY).is_ok());
        l.tick(0, T0 + 50_000);
        l.tick(1, T0 + 50_000);
        assert!(l.check(T0 + 100_000, STALL, BUSY).is_ok());
    }

    #[test]
    fn a_consumer_that_stops_looping_is_reported() {
        let l = PoolLiveness::new(2, T0);
        l.tick(0, T0 + 100_000);
        let err = l.check(T0 + 100_000, STALL, BUSY).unwrap_err();
        assert!(err.contains("consommateur 1 figé"), "{err}");
        assert!(!err.contains("consommateur 0"));
    }

    #[test]
    fn a_long_run_is_alive_until_the_run_timeout() {
        let l = PoolLiveness::new(1, T0);
        l.busy(0, T0);
        // Bien au-delà du seuil de boucle, mais sous le délai d'un run.
        assert!(l.check(T0 + 30 * 60_000, STALL, BUSY).is_ok());
        let err = l.check(T0 + BUSY, STALL, BUSY).unwrap_err();
        assert!(err.contains("occupé"), "{err}");
        // Fin du run : de nouveau au repos et frais.
        l.idle(0, T0 + BUSY + 1);
        assert!(l.check(T0 + BUSY + 2, STALL, BUSY).is_ok());
    }

    #[test]
    fn unknown_consumer_ids_are_ignored() {
        let l = PoolLiveness::new(1, T0);
        l.tick(7, T0);
        l.busy(7, T0);
        l.idle(7, T0);
        assert!(l.check(T0, STALL, BUSY).is_ok());
    }
}

//! Trigger Scheduler (F3.3) — déclenchement périodique.
//!
//! Deux composants déterministes, testables sans temps réel :
//! - [`ScheduleTrigger`] : déclenche à **intervalle**, horloge injectée
//!   (secondes Unix) ; implémente [`autome_core::Trigger`].
//! - [`CronSchedule`] : parseur d'expression cron 5 champs + `matches`.
//!
//! La conversion horloge murale → champs cron (dépendance date) et le
//! câblage du planificateur relèvent de F5.3.

use autome_core::{Trigger, TriggerEvent};

/// Déclencheur à intervalle fixe. `C` fournit l'heure courante (secondes
/// Unix) — injectable pour des tests déterministes.
pub struct ScheduleTrigger<C: FnMut() -> u64> {
    every_secs: u64,
    last_fired: Option<u64>,
    clock: C,
}

impl<C: FnMut() -> u64> ScheduleTrigger<C> {
    /// Crée un trigger qui se déclenche toutes les `every_secs` secondes.
    pub fn new(every_secs: u64, clock: C) -> Self {
        Self {
            every_secs: every_secs.max(1),
            last_fired: None,
            clock,
        }
    }
}

impl<C: FnMut() -> u64> Trigger for ScheduleTrigger<C> {
    fn kind(&self) -> &'static str {
        "schedule"
    }

    fn poll(&mut self) -> autome_core::Result<Option<TriggerEvent>> {
        let now = (self.clock)();
        let due = match self.last_fired {
            None => true,
            Some(last) => now.saturating_sub(last) >= self.every_secs,
        };
        if due {
            self.last_fired = Some(now);
            Ok(Some(TriggerEvent::new(
                "schedule",
                serde_json::json!({ "fired_at": now }),
            )))
        } else {
            Ok(None)
        }
    }
}

/// Un champ cron : `*`, valeur exacte, pas `*/n`, ou liste de valeurs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Any,
    Step(u32),
    Values(Vec<u32>),
}

impl Field {
    fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw == "*" {
            return Ok(Field::Any);
        }
        if let Some(step) = raw.strip_prefix("*/") {
            let n: u32 = step
                .parse()
                .map_err(|_| format!("pas de cron invalide : `{raw}`"))?;
            if n == 0 {
                return Err("pas de cron nul".into());
            }
            return Ok(Field::Step(n));
        }
        let values = raw
            .split(',')
            .map(|v| {
                v.trim()
                    .parse::<u32>()
                    .map_err(|_| format!("champ cron invalide : `{raw}`"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Field::Values(values))
    }

    fn matches(&self, value: u32) -> bool {
        match self {
            Field::Any => true,
            Field::Step(n) => value % n == 0,
            Field::Values(vs) => vs.contains(&value),
        }
    }
}

/// Instant décomposé comparé à une [`CronSchedule`].
#[derive(Debug, Clone, Copy)]
pub struct CronTime {
    /// Minute (0–59).
    pub minute: u32,
    /// Heure (0–23).
    pub hour: u32,
    /// Jour du mois (1–31).
    pub day: u32,
    /// Mois (1–12).
    pub month: u32,
    /// Jour de la semaine (0 = dimanche … 6 = samedi).
    pub weekday: u32,
}

/// Expression cron à 5 champs : `minute heure jour mois jour_semaine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: Field,
    hour: Field,
    day: Field,
    month: Field,
    weekday: Field,
}

impl CronSchedule {
    /// Parse une expression cron standard à 5 champs.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "cron : 5 champs attendus, {} reçus dans `{expr}`",
                parts.len()
            ));
        }
        Ok(Self {
            minute: Field::parse(parts[0])?,
            hour: Field::parse(parts[1])?,
            day: Field::parse(parts[2])?,
            month: Field::parse(parts[3])?,
            weekday: Field::parse(parts[4])?,
        })
    }

    /// `true` si `t` satisfait tous les champs de l'expression.
    pub fn matches(&self, t: &CronTime) -> bool {
        self.minute.matches(t.minute)
            && self.hour.matches(t.hour)
            && self.day.matches(t.day)
            && self.month.matches(t.month)
            && self.weekday.matches(t.weekday)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_fires_first_then_waits() {
        let times = [100u64, 130, 160, 161];
        let mut i = 0;
        let mut trig = ScheduleTrigger::new(60, || {
            let v = times[i];
            i += 1;
            v
        });
        // t=100 : premier poll → déclenche.
        assert!(trig.poll().unwrap().is_some());
        // t=130 : 30s < 60s → rien.
        assert!(trig.poll().unwrap().is_none());
        // t=160 : 60s écoulées → déclenche.
        let ev = trig.poll().unwrap().unwrap();
        assert_eq!(ev.source, "schedule");
        assert_eq!(ev.payload, serde_json::json!({ "fired_at": 160 }));
        // t=161 : 1s depuis dernier → rien.
        assert!(trig.poll().unwrap().is_none());
        assert_eq!(trig.kind(), "schedule");
    }

    #[test]
    fn cron_parses_and_matches() {
        let c = CronSchedule::parse("*/15 9 * * 1").unwrap();
        let base = CronTime {
            minute: 0,
            hour: 9,
            day: 12,
            month: 5,
            weekday: 1,
        };
        assert!(c.matches(&base)); // 0 % 15 == 0, 9h, lundi
        assert!(c.matches(&CronTime { minute: 30, ..base }));
        assert!(!c.matches(&CronTime { minute: 31, ..base })); // 31 % 15 != 0
        assert!(!c.matches(&CronTime { hour: 10, ..base })); // heure != 9
        assert!(!c.matches(&CronTime { weekday: 2, ..base })); // pas lundi
    }

    #[test]
    fn cron_supports_lists_and_wildcards() {
        let c = CronSchedule::parse("0,30 * * * *").unwrap();
        let t = CronTime {
            minute: 30,
            hour: 23,
            day: 1,
            month: 1,
            weekday: 0,
        };
        assert!(c.matches(&t));
        assert!(!c.matches(&CronTime { minute: 15, ..t }));
    }

    #[test]
    fn cron_rejects_malformed_expressions() {
        assert!(CronSchedule::parse("* * * *").is_err()); // 4 champs
        assert!(CronSchedule::parse("bad * * * *").is_err());
        assert!(CronSchedule::parse("*/0 * * * *").is_err());
    }
}

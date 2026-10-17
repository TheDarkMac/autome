//! Trigger Scheduler (F3.3) — déclenchement périodique.
//!
//! Deux composants déterministes, testables sans temps réel :
//! - [`ScheduleTrigger`] : déclenche à **intervalle**, horloge injectée
//!   (secondes Unix) ; implémente [`autome_core::Trigger`].
//! - [`CronSchedule`] : parseur d'expression cron 5 champs, `matches` et
//!   calcul de la prochaine échéance ([`CronSchedule::next_after`], UTC).
//!
//! Le câblage (échéances partagées entre répliques, boucle du worker) vit
//! dans [`crate::triggers`] et `autome_worker::triggers` (#227).

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

/// Un champ cron : ensemble des valeurs autorisées (bit `v` = valeur `v`).
///
/// Syntaxe d'un élément (séparés par `,`) : `*`, `v`, `a-b`, avec un pas
/// facultatif `/n` (`*/15`, `10-50/10`, `5/20` = de 5 au maximum par 20).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    allowed: u64,
    /// Le champ commence par `*` : sert à la règle jour du mois / jour de
    /// semaine (cf. [`CronSchedule::matches`]).
    star: bool,
}

impl Field {
    fn parse(raw: &str, name: &str, min: u32, max: u32) -> Result<Self, String> {
        let raw = raw.trim();
        let mut allowed = 0u64;
        for item in raw.split(',') {
            let (range, step) = match item.split_once('/') {
                Some((range, step)) => {
                    let n: u32 = step
                        .parse()
                        .map_err(|_| format!("cron : pas invalide `{item}` ({name})"))?;
                    if n == 0 {
                        return Err(format!("cron : pas nul `{item}` ({name})"));
                    }
                    (range, Some(n))
                }
                None => (item, None),
            };
            let value = |v: &str| -> Result<u32, String> {
                let v: u32 = v
                    .parse()
                    .map_err(|_| format!("cron : valeur invalide `{item}` ({name})"))?;
                if v < min || v > max {
                    return Err(format!(
                        "cron : `{item}` hors bornes ({name} : {min}–{max})"
                    ));
                }
                Ok(v)
            };
            let (lo, hi) = if range == "*" {
                (min, max)
            } else if let Some((a, b)) = range.split_once('-') {
                let (a, b) = (value(a)?, value(b)?);
                if a > b {
                    return Err(format!("cron : plage inversée `{item}` ({name})"));
                }
                (a, b)
            } else {
                let v = value(range)?;
                // `5/20` : de 5 jusqu'au maximum, par pas de 20.
                (v, if step.is_some() { max } else { v })
            };
            for v in (lo..=hi).step_by(step.unwrap_or(1) as usize) {
                allowed |= 1 << v;
            }
        }
        Ok(Self {
            allowed,
            star: raw.starts_with('*'),
        })
    }

    fn matches(&self, value: u32) -> bool {
        value < 64 && self.allowed & (1 << value) != 0
    }
}

/// Instant décomposé comparé à une [`CronSchedule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Date civile (année, mois 1–12, jour 1–31) du jour `days` depuis le
/// 1970-01-01 (calendrier grégorien proleptique, algorithme de H. Hinnant).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Nombre de jours depuis le 1970-01-01 de la date civile donnée.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl CronTime {
    /// Décompose un horodatage Unix (ms) en UTC.
    pub fn from_unix_ms(ms: i64) -> Self {
        let days = ms.div_euclid(DAY_MS);
        let in_day = ms.rem_euclid(DAY_MS);
        let (_, month, day) = civil_from_days(days);
        Self {
            minute: ((in_day % HOUR_MS) / MINUTE_MS) as u32,
            hour: (in_day / HOUR_MS) as u32,
            day,
            month,
            // Le 1970-01-01 était un jeudi (4).
            weekday: (days + 4).rem_euclid(7) as u32,
        }
    }
}

/// Horodatage Unix (ms) au format RFC 3339 UTC, à la seconde
/// (`2026-10-17T09:00:00Z`).
pub fn rfc3339_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1_000);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let in_day = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        in_day / 3_600,
        (in_day % 3_600) / 60,
        in_day % 60
    )
}

/// Horizon de recherche de la prochaine échéance : au-delà, l'expression est
/// considérée comme ne se déclenchant jamais (ex. `0 0 30 2 *`). Couvre le
/// 29 février (cycle de 4 ans, 8 ans autour d'un siècle non bissextile).
const SEARCH_HORIZON_MS: i64 = 9 * 366 * DAY_MS;

/// Expression cron à 5 champs : `minute heure jour mois jour_semaine`,
/// évaluée en UTC.
///
/// Comme le cron Unix, si le jour du mois **et** le jour de la semaine sont
/// restreints (ne commencent pas par `*`), un jour satisfaisant **l'un ou
/// l'autre** convient (`0 0 1 * 1` : le 1er du mois et chaque lundi).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: Field,
    hour: Field,
    day: Field,
    month: Field,
    weekday: Field,
}

impl CronSchedule {
    /// Parse une expression cron standard à 5 champs. Jour de semaine : 0–7
    /// (0 et 7 = dimanche).
    pub fn parse(expr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "cron : 5 champs attendus, {} reçus dans `{expr}`",
                parts.len()
            ));
        }
        let mut weekday = Field::parse(parts[4], "jour de semaine", 0, 7)?;
        if weekday.matches(7) {
            weekday.allowed = (weekday.allowed & !(1 << 7)) | 1;
        }
        Ok(Self {
            minute: Field::parse(parts[0], "minute", 0, 59)?,
            hour: Field::parse(parts[1], "heure", 0, 23)?,
            day: Field::parse(parts[2], "jour", 1, 31)?,
            month: Field::parse(parts[3], "mois", 1, 12)?,
            weekday,
        })
    }

    fn day_matches(&self, t: &CronTime) -> bool {
        let day = self.day.matches(t.day);
        let weekday = self.weekday.matches(t.weekday);
        if self.day.star || self.weekday.star {
            day && weekday
        } else {
            day || weekday
        }
    }

    /// `true` si `t` satisfait l'expression.
    pub fn matches(&self, t: &CronTime) -> bool {
        self.minute.matches(t.minute)
            && self.hour.matches(t.hour)
            && self.month.matches(t.month)
            && self.day_matches(t)
    }

    /// Première minute (horodatage Unix en ms, UTC) **strictement après**
    /// `after_ms` qui satisfait l'expression ; `None` si aucune dans
    /// l'horizon de recherche (expression qui ne se déclenche jamais).
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let limit = after_ms.saturating_add(SEARCH_HORIZON_MS);
        let mut t = after_ms.div_euclid(MINUTE_MS) * MINUTE_MS + MINUTE_MS;
        while t <= limit {
            let c = CronTime::from_unix_ms(t);
            if !self.month.matches(c.month) {
                // Premier jour du mois suivant, 00:00.
                let (year, month, _) = civil_from_days(t.div_euclid(DAY_MS));
                let (year, month) = if month == 12 {
                    (year + 1, 1)
                } else {
                    (year, month + 1)
                };
                t = days_from_civil(year, month, 1) * DAY_MS;
            } else if !self.day_matches(&c) {
                t = (t.div_euclid(DAY_MS) + 1) * DAY_MS;
            } else if !self.hour.matches(c.hour) {
                t = (t.div_euclid(HOUR_MS) + 1) * HOUR_MS;
            } else if !self.minute.matches(c.minute) {
                t += MINUTE_MS;
            } else {
                return Some(t);
            }
        }
        None
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
        // Bornes de chaque champ (#227 : `99 * * * *` était accepté).
        for expr in [
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 0 *",
            "* * * 13 *",
            "* * * * 8",
            "5-1 * * * *",
            "1- * * * *",
            "*/x * * * *",
        ] {
            assert!(CronSchedule::parse(expr).is_err(), "{expr} accepté");
        }
    }

    /// Horodatage Unix (ms, UTC) d'une date/heure civile.
    fn at(year: i64, month: u32, day: u32, hour: i64, minute: i64) -> i64 {
        days_from_civil(year, month, day) * DAY_MS + hour * HOUR_MS + minute * MINUTE_MS
    }

    #[test]
    fn cron_supports_ranges_and_steps_from_field_start() {
        let c = CronSchedule::parse("10-50/20 9-17 */2 * 1-5").unwrap();
        let t = CronTime {
            minute: 30,
            hour: 9,
            day: 3,
            month: 6,
            weekday: 3,
        };
        assert!(c.matches(&t));
        assert!(!c.matches(&CronTime { minute: 20, ..t })); // 10, 30, 50
        assert!(!c.matches(&CronTime { hour: 18, ..t }));
        // `*/2` sur le jour du mois part de 1 : 1, 3, 5…
        assert!(!c.matches(&CronTime { day: 2, ..t }));
        assert!(c.matches(&CronTime { day: 1, ..t }));
        assert!(!c.matches(&CronTime { weekday: 0, ..t })); // dimanche
                                                            // `5/20` : de 5 au maximum, par 20.
        let c = CronSchedule::parse("5/20 * * * *").unwrap();
        assert!(c.matches(&CronTime { minute: 45, ..t }));
        assert!(!c.matches(&CronTime { minute: 0, ..t }));
    }

    #[test]
    fn cron_sunday_is_zero_or_seven() {
        let sunday = CronTime {
            minute: 0,
            hour: 0,
            day: 7,
            month: 6,
            weekday: 0,
        };
        assert!(CronSchedule::parse("0 0 * * 7").unwrap().matches(&sunday));
        assert!(CronSchedule::parse("0 0 * * 0").unwrap().matches(&sunday));
        assert!(CronSchedule::parse("0 0 * * 5-7").unwrap().matches(&sunday));
    }

    #[test]
    fn cron_day_of_month_or_weekday_when_both_restricted() {
        // Le 1er du mois **ou** chaque lundi (règle du cron Unix).
        let c = CronSchedule::parse("0 0 1 * 1").unwrap();
        let t = CronTime {
            minute: 0,
            hour: 0,
            day: 1,
            month: 6,
            weekday: 4,
        };
        assert!(c.matches(&t));
        assert!(c.matches(&CronTime {
            day: 9,
            weekday: 1,
            ..t
        }));
        assert!(!c.matches(&CronTime {
            day: 9,
            weekday: 2,
            ..t
        }));
        // Un seul restreint : ET (comportement habituel).
        let c = CronSchedule::parse("0 0 * * 1").unwrap();
        assert!(!c.matches(&t));
    }

    #[test]
    fn unix_time_decomposes_in_utc() {
        assert_eq!(
            CronTime::from_unix_ms(0),
            CronTime {
                minute: 0,
                hour: 0,
                day: 1,
                month: 1,
                weekday: 4, // jeudi
            }
        );
        // 2024-02-29 23:59 UTC, un jeudi.
        let t = CronTime::from_unix_ms(at(2024, 2, 29, 23, 59) + 59_999);
        assert_eq!(
            (t.day, t.month, t.hour, t.minute, t.weekday),
            (29, 2, 23, 59, 4)
        );
        assert_eq!(at(2024, 2, 29, 0, 0), 1_709_164_800_000);
        assert_eq!(at(1969, 12, 31, 0, 0), -DAY_MS);
    }

    #[test]
    fn rfc3339_formats_utc_seconds() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(at(2024, 2, 29, 9, 5) + 7_999), "2024-02-29T09:05:07Z");
        assert_eq!(rfc3339_utc(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn next_after_is_strictly_later_and_aligned_on_the_minute() {
        let every = CronSchedule::parse("* * * * *").unwrap();
        let t = at(2026, 10, 17, 10, 0);
        assert_eq!(every.next_after(t), Some(t + MINUTE_MS));
        assert_eq!(every.next_after(t + 1), Some(t + MINUTE_MS));
        assert_eq!(every.next_after(t - 1), Some(t));

        let hourly = CronSchedule::parse("0 * * * *").unwrap();
        assert_eq!(hourly.next_after(t), Some(at(2026, 10, 17, 11, 0)));
        assert_eq!(hourly.next_after(t + 5_000), Some(at(2026, 10, 17, 11, 0)));
    }

    #[test]
    fn next_after_crosses_days_months_and_years() {
        let c = CronSchedule::parse("30 8 * * 1-5").unwrap();
        // Vendredi 2026-10-16 09:00 ⇒ lundi 2026-10-19 08:30.
        assert_eq!(
            c.next_after(at(2026, 10, 16, 9, 0)),
            Some(at(2026, 10, 19, 8, 30))
        );
        let new_year = CronSchedule::parse("0 0 1 1 *").unwrap();
        assert_eq!(
            new_year.next_after(at(2026, 10, 17, 0, 0)),
            Some(at(2027, 1, 1, 0, 0))
        );
        let end_of_month = CronSchedule::parse("15 23 31 * *").unwrap();
        // Novembre n'a pas de 31 : décembre.
        assert_eq!(
            end_of_month.next_after(at(2026, 11, 1, 0, 0)),
            Some(at(2026, 12, 31, 23, 15))
        );
    }

    #[test]
    fn next_after_finds_leap_days_and_detects_impossible_dates() {
        let leap = CronSchedule::parse("0 12 29 2 *").unwrap();
        assert_eq!(
            leap.next_after(at(2026, 10, 17, 0, 0)),
            Some(at(2028, 2, 29, 12, 0))
        );
        // 2100 n'est pas bissextile : 2096 puis 2104.
        assert_eq!(
            leap.next_after(at(2096, 3, 1, 0, 0)),
            Some(at(2104, 2, 29, 12, 0))
        );
        assert_eq!(
            CronSchedule::parse("0 0 30 2 *").unwrap().next_after(0),
            None
        );
    }
}

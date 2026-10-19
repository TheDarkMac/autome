//! Primitives du format d'exposition texte Prometheus (v0.0.4).
//!
//! Partagées par les registres sans état global de l'API (F10.6.a) et du
//! worker (#235) : chacun garde ses propres séries et s'appuie ici sur le
//! histogram cumulatif, l'échappement des labels et le format des flottants.

use std::fmt::Write as _;

/// `Content-Type` d'une réponse `/metrics`.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Buckets par défaut de Prometheus (`prometheus.DefBuckets`), en secondes :
/// adaptés aux latences de requêtes HTTP.
pub const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Histogram cumulatif sur des bornes supérieures (`le`) fixées à la création.
///
/// `le_counts[i]` compte les observations `<= bounds[i]` (déjà cumulatif :
/// chaque observation incrémente tous les buckets qui la couvrent). `count`
/// est le total (équivaut au bucket `+Inf`) et `sum` la somme des valeurs.
#[derive(Debug, Clone)]
pub struct Histogram {
    bounds: &'static [f64],
    le_counts: Vec<u64>,
    count: u64,
    sum: f64,
}

impl Histogram {
    /// Histogram vide sur les bornes `bounds` (croissantes).
    pub fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            le_counts: vec![0; bounds.len()],
            count: 0,
            sum: 0.0,
        }
    }

    /// Enregistre une observation.
    pub fn observe(&mut self, value: f64) {
        for (i, &bound) in self.bounds.iter().enumerate() {
            if value <= bound {
                self.le_counts[i] += 1;
            }
        }
        self.count += 1;
        self.sum += value;
    }

    /// Nombre d'observations.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Écrit les lignes `<name>_bucket`, `<name>_sum` et `<name>_count` de la
    /// série. `labels` : labels déjà formatés et échappés (`a="x",b="y"`), ou
    /// vide.
    pub fn write(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        let mut bucket = |le: &str, n: u64| {
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"{le}\"}} {n}");
        };
        for (i, &bound) in self.bounds.iter().enumerate() {
            bucket(&fmt_f64(bound), self.le_counts[i]);
        }
        bucket("+Inf", self.count);
        let braced = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        let _ = writeln!(out, "{name}_sum{braced} {}", fmt_f64(self.sum));
        let _ = writeln!(out, "{name}_count{braced} {}", self.count);
    }
}

/// Écrit les lignes `# HELP` et `# TYPE` d'une métrique.
pub fn write_header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Échappe une valeur de label selon le format Prometheus : `\`, `"` et saut
/// de ligne. Les labels du projet n'en contiennent normalement pas, mais le
/// rendu doit rester valide quoi qu'il arrive.
pub fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Formate un flottant sans notation scientifique ni `.0` superflu
/// (`1.0` → `1`, `0.005` → `0.005`), conforme aux valeurs `le` Prometheus.
pub fn fmt_f64(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_cumulative_with_and_without_labels() {
        let mut h = Histogram::new(&[0.5, 1.0]);
        h.observe(0.2);
        h.observe(0.7);
        h.observe(3.0);
        assert_eq!(h.count(), 3);

        let mut out = String::new();
        h.write(&mut out, "x_seconds", "k=\"v\"");
        assert_eq!(
            out,
            "x_seconds_bucket{k=\"v\",le=\"0.5\"} 1\n\
             x_seconds_bucket{k=\"v\",le=\"1\"} 2\n\
             x_seconds_bucket{k=\"v\",le=\"+Inf\"} 3\n\
             x_seconds_sum{k=\"v\"} 3.9\n\
             x_seconds_count{k=\"v\"} 3\n"
        );

        let mut out = String::new();
        h.write(&mut out, "x_seconds", "");
        assert!(out.starts_with("x_seconds_bucket{le=\"0.5\"} 1\n"));
        assert!(out.ends_with("x_seconds_sum 3.9\nx_seconds_count 3\n"));
    }

    #[test]
    fn header_lines() {
        let mut out = String::new();
        write_header(&mut out, "x_total", "counter", "Things.");
        assert_eq!(out, "# HELP x_total Things.\n# TYPE x_total counter\n");
    }

    #[test]
    fn labels_are_escaped() {
        assert_eq!(escape_label("/a\"b\\c\nd"), "/a\\\"b\\\\c\\nd");
    }

    #[test]
    fn fmt_f64_drops_trailing_zeros() {
        assert_eq!(fmt_f64(1.0), "1");
        assert_eq!(fmt_f64(0.005), "0.005");
        assert_eq!(fmt_f64(2.5), "2.5");
        assert_eq!(fmt_f64(0.202), "0.202");
        assert_eq!(fmt_f64(3600.0), "3600");
    }
}

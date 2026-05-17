//! Abstraction des déclencheurs de workflow (F3.1).
//!
//! Un *trigger* produit des [`TriggerEvent`] qui démarrent un run. Le
//! contrat est **pull** : l'orchestrateur appelle [`Trigger::poll`]
//! périodiquement. Les triggers *push* (webhook — F3.2) enfilent les
//! événements reçus en interne et les restituent via `poll`.
//!
//! `core` reste agnostique : aucune dépendance réseau/horloge ici. Les
//! implémentations concrètes vivent dans `connectors` (F3.2–F3.4).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;

/// Événement émis par un trigger : ce qui démarre (et alimente) un run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerEvent {
    /// Type du trigger source (ex. `"webhook"`, `"schedule"`).
    pub source: String,
    /// Charge utile exposée au workflow (sortie du node trigger).
    pub payload: Value,
}

impl TriggerEvent {
    /// Construit un événement à partir d'un type source et d'un payload.
    pub fn new(source: impl Into<String>, payload: Value) -> Self {
        Self {
            source: source.into(),
            payload,
        }
    }
}

/// Contrat commun à tous les déclencheurs (extensible).
pub trait Trigger {
    /// Identifiant stable du type de trigger (ex. `"webhook"`).
    fn kind(&self) -> &'static str;

    /// Récupère le prochain événement en attente, le cas échéant.
    ///
    /// `Ok(None)` = rien à déclencher pour l'instant (pas une erreur).
    fn poll(&mut self) -> Result<Option<TriggerEvent>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trigger factice : émet `n` événements puis plus rien.
    struct Countdown {
        remaining: u32,
    }
    impl Trigger for Countdown {
        fn kind(&self) -> &'static str {
            "countdown"
        }
        fn poll(&mut self) -> Result<Option<TriggerEvent>> {
            if self.remaining == 0 {
                return Ok(None);
            }
            self.remaining -= 1;
            Ok(Some(TriggerEvent::new(
                self.kind(),
                serde_json::json!({ "left": self.remaining }),
            )))
        }
    }

    #[test]
    fn polls_until_exhausted() {
        let mut t = Countdown { remaining: 2 };
        assert_eq!(t.kind(), "countdown");
        assert!(t.poll().unwrap().is_some());
        let last = t.poll().unwrap().unwrap();
        assert_eq!(last.source, "countdown");
        assert_eq!(last.payload, serde_json::json!({ "left": 0 }));
        assert!(t.poll().unwrap().is_none());
    }

    #[test]
    fn event_roundtrips_through_json() {
        let e = TriggerEvent::new("webhook", serde_json::json!({ "a": 1 }));
        let s = serde_json::to_string(&e).unwrap();
        let back: TriggerEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(e, back);
    }
}

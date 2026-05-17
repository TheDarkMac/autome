//! `autome-connectors` — intégrations tierces.
//!
//! Dépend de `core` uniquement (ARCHITECTURE.md §9). Le trait `Connector`
//! et les implémentations concrètes (Slack, Gmail, Notion, GitHub, HTTP)
//! sont livrés au domaine 4 du roadmap (F4.*). Squelette : F1.3.e.

pub mod polling;
pub mod schedule;
pub mod webhook;

pub use polling::PollingTrigger;
pub use schedule::{CronSchedule, CronTime, ScheduleTrigger};
pub use webhook::WebhookTrigger;

use autome_core::Result;

/// Contrat minimal d'un connecteur tiers.
///
/// Volontairement réduit ici ; le cycle de vie d'auth, le rate-limit et la
/// déclaration des actions sont spécifiés en F4.5 (SDK interne connecteur).
pub trait Connector {
    /// Identifiant stable du provider (ex. `"slack"`, `"github"`).
    fn provider(&self) -> &'static str;

    /// Vérifie que le connecteur est correctement configuré/authentifié.
    fn health_check(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;
    impl Connector for Dummy {
        fn provider(&self) -> &'static str {
            "dummy"
        }
    }

    #[test]
    fn default_health_check_is_ok() {
        let c = Dummy;
        assert_eq!(c.provider(), "dummy");
        assert!(c.health_check().is_ok());
    }
}

//! `autome-connectors` — intégrations tierces.
//!
//! Dépend de `core` uniquement (ARCHITECTURE.md §9). Le trait `Connector`
//! et les implémentations concrètes (Slack, Gmail, Notion, GitHub, HTTP)
//! sont livrés au domaine 4 du roadmap (F4.*). Squelette : F1.3.e.

pub mod github;
pub mod gmail;
pub mod http;
pub mod notion;
pub mod polling;
pub mod schedule;
pub mod slack;
pub mod webhook;

pub use github::{
    verify_github_signature, GithubAuth, GithubCloseIssue, GithubCommentIssue, GithubCreateIssue,
    GithubCreatePull, GithubListPulls, GithubMergePull,
};
pub use gmail::{
    GmailAuth, GmailGetAttachment, GmailListMessages, GmailReadMessage, GmailSendEmail,
};
pub use http::{HttpAction, HttpClient, HttpRequest, HttpResponse};
pub use notion::{
    extract_plain_text, heading_block, text_block, NotionAuth, NotionCreatePage,
    NotionQueryDatabase, NotionReadPage,
};
pub use polling::PollingTrigger;
pub use schedule::{CronSchedule, CronTime, ScheduleTrigger};
pub use slack::{SlackAuth, SlackReadChannel, SlackSendMessage};
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

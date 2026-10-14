//! `autome-connectors` — intégrations tierces.
//!
//! Dépend de `core` uniquement (ARCHITECTURE.md §9). Le trait `Connector`
//! et les implémentations concrètes (Slack, Gmail, Notion, GitHub, HTTP)
//! relèvent du domaine 4 du roadmap (F4.*). Le SDK transversal vit dans
//! [`sdk`] (F4.5) ; un connecteur exemple complet est livré dans
//! [`template`].

pub mod credentials;
pub mod egress;
pub mod github;
pub mod gmail;
pub mod http;
pub mod notion;
pub mod oauth;
pub mod polling;
pub mod reqwest_client;
pub mod schedule;
pub mod sdk;
pub mod slack;
pub mod template;
pub mod webhook;

pub use credentials::{CredentialService, CredentialSummary, OAuthCredentialService, ProviderInfo};
pub use egress::{is_public_ip, EgressError, EgressPolicy};

pub use github::{
    verify_github_signature, GithubAuth, GithubCloseIssue, GithubCommentIssue, GithubConnector,
    GithubCreateIssue, GithubCreatePull, GithubListPulls, GithubMergePull,
};
pub use gmail::{
    GmailAuth, GmailConnector, GmailGetAttachment, GmailListMessages, GmailReadMessage,
    GmailSendEmail,
};
pub use http::{HttpAction, HttpClient, HttpRequest, HttpResponse};
pub use notion::{
    extract_plain_text, heading_block, text_block, NotionAuth, NotionConnector, NotionCreatePage,
    NotionQueryDatabase, NotionReadPage,
};
pub use polling::PollingTrigger;
pub use reqwest_client::ReqwestHttpClient;
pub use schedule::{CronSchedule, CronTime, ScheduleTrigger};
pub use sdk::{
    ActionSchema, Connector, Credentials, ParamKind, ParamSpec, RecordingClient, RetryPolicy,
    RetryingClient,
};
pub use slack::{SlackAuth, SlackConnector, SlackReadChannel, SlackSendMessage};
pub use template::{EchoAction, EchoAuth, EchoConnector};
pub use webhook::WebhookTrigger;

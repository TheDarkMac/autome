//! Dispatch des actions de connecteurs (#159).
//!
//! Traduit le `action_type` d'un node (`"slack.send_message"`,
//! `"gmail.send_email"`, `"notion.create_page"`, `"github.create_issue"`, …)
//! vers l'action de connecteur correspondante (`crate`
//! `autome-connectors`), **injecte le token** du provider résolu par un
//! [`CredentialResolver`], puis exécute l'action.
//!
//! Le **provider** est le préfixe du `action_type` (`slack.*` → `slack`). Le
//! token devient l'auth bearer attendue par chaque connecteur
//! ([`SlackAuth::Bot`], [`GmailAuth::OAuth2`], [`NotionAuth::Integration`],
//! [`GithubAuth::Token`]).
//!
//! ## Résolution des credentials — mono-tenant pour l'instant
//!
//! Les runs ne portent **pas** de tenant/utilisateur (la migration `0009` a
//! retiré `owner_id`) et `CredentialService` est *metadata-only* par design :
//! la résolution **par utilisateur** (store chiffré PostgreSQL + tenant sur le
//! run) reste un follow-up. L'impl par défaut [`EnvCredentialResolver`] lit un
//! token **par provider** dans l'environnement
//! (`AUTOME_CONNECTOR_TOKEN_<PROVIDER>`), ce qui débloque un déploiement
//! mono-tenant et reste testable sans base. Le trait permet de brancher une
//! résolution store-backed plus tard sans toucher au runner.

use autome_connectors::{
    GithubAuth, GithubCloseIssue, GithubCommentIssue, GithubCreateIssue, GithubCreatePull,
    GithubListPulls, GithubMergePull, GmailAuth, GmailGetAttachment, GmailListMessages,
    GmailReadMessage, GmailSendEmail, HttpClient, NotionAuth, NotionCreatePage,
    NotionQueryDatabase, NotionReadPage, SlackAuth, SlackReadChannel, SlackSendMessage,
};
use autome_core::Action;
use serde_json::Value;

/// Résout le token d'un provider de connecteur (`"slack"`, `"gmail"`, …).
///
/// `Send + Sync` : le résolveur est partagé (`Arc`) et utilisé dans le
/// `spawn_blocking` du worker.
pub trait CredentialResolver: Send + Sync {
    /// Token du `provider`, ou `None` si non configuré. `Err` pour une erreur
    /// d'accès au backend (la décision retry/DLQ revient à l'appelant).
    fn token(&self, provider: &str) -> Result<Option<String>, String>;
}

/// Résolveur **mono-tenant** par variables d'environnement :
/// `AUTOME_CONNECTOR_TOKEN_<PROVIDER>` (provider en majuscules).
///
/// Ex. `AUTOME_CONNECTOR_TOKEN_SLACK=xoxb-…`,
/// `AUTOME_CONNECTOR_TOKEN_GITHUB=ghp_…`.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvCredentialResolver;

impl CredentialResolver for EnvCredentialResolver {
    fn token(&self, provider: &str) -> Result<Option<String>, String> {
        let var = format!("AUTOME_CONNECTOR_TOKEN_{}", provider.to_ascii_uppercase());
        Ok(std::env::var(&var).ok().filter(|v| !v.is_empty()))
    }
}

/// `true` si le `action_type` est dispatché par ce module (connecteur connu).
/// `http_request` reste géré directement par le runner (pas de credentials).
pub fn is_connector_action(action_type: &str) -> bool {
    matches!(
        action_type.split('.').next().unwrap_or(""),
        "slack" | "gmail" | "notion" | "github"
    )
}

/// Exécute l'action de connecteur `action_type` avec `params`, en injectant le
/// token résolu par `resolver`. `http` (clonable) est le transport partagé.
///
/// Erreurs (toutes en `String`, l'appelant décide du verdict) :
/// - provider/`action_type` inconnu → message explicite ;
/// - token absent → message indiquant la variable à définir ;
/// - erreur d'exécution du connecteur → message du connecteur.
pub fn dispatch_action<C, R>(
    action_type: &str,
    params: &Value,
    http: &C,
    resolver: &R,
) -> Result<Value, String>
where
    C: HttpClient + Clone,
    R: CredentialResolver + ?Sized,
{
    let provider = action_type.split('.').next().unwrap_or("");
    // Token requis par tous les connecteurs supportés.
    let token = resolver.token(provider)?.ok_or_else(|| {
        format!(
            "credential `{provider}` absente (définir AUTOME_CONNECTOR_TOKEN_{})",
            provider.to_ascii_uppercase()
        )
    })?;
    let run = |r: autome_core::Result<Value>| r.map_err(|e| e.to_string());

    match action_type {
        // ───── Slack ─────
        "slack.send_message" => run(SlackSendMessage::new(SlackAuth::Bot(token), http.clone()).execute(params)),
        "slack.read_channel" => run(SlackReadChannel::new(SlackAuth::Bot(token), http.clone()).execute(params)),
        // ───── Gmail ─────
        "gmail.send_email" => run(GmailSendEmail::new(GmailAuth::OAuth2(token), http.clone()).execute(params)),
        "gmail.list_messages" => run(GmailListMessages::new(GmailAuth::OAuth2(token), http.clone()).execute(params)),
        "gmail.read_message" => run(GmailReadMessage::new(GmailAuth::OAuth2(token), http.clone()).execute(params)),
        "gmail.get_attachment" => run(GmailGetAttachment::new(GmailAuth::OAuth2(token), http.clone()).execute(params)),
        // ───── Notion ─────
        "notion.create_page" => run(NotionCreatePage::new(NotionAuth::Integration(token), http.clone()).execute(params)),
        "notion.read_page" => run(NotionReadPage::new(NotionAuth::Integration(token), http.clone()).execute(params)),
        "notion.query_database" => run(NotionQueryDatabase::new(NotionAuth::Integration(token), http.clone()).execute(params)),
        // ───── GitHub ─────
        "github.create_issue" => run(GithubCreateIssue::new(GithubAuth::Token(token), http.clone()).execute(params)),
        "github.comment_issue" => run(GithubCommentIssue::new(GithubAuth::Token(token), http.clone()).execute(params)),
        "github.close_issue" => run(GithubCloseIssue::new(GithubAuth::Token(token), http.clone()).execute(params)),
        "github.list_pulls" => run(GithubListPulls::new(GithubAuth::Token(token), http.clone()).execute(params)),
        "github.create_pull" => run(GithubCreatePull::new(GithubAuth::Token(token), http.clone()).execute(params)),
        "github.merge_pull" => run(GithubMergePull::new(GithubAuth::Token(token), http.clone()).execute(params)),
        other => Err(format!(
            "action `{other}` non supportée par le worker (connecteurs : slack.*, gmail.*, notion.*, github.*, http_request)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_connectors::{HttpRequest, HttpResponse};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// Mock `HttpClient` **clonable** (état partagé via `Arc<Mutex>`) : le clone
    /// consommé par l'action partage l'enregistrement des requêtes envoyées.
    #[derive(Clone, Default)]
    struct SharedMock {
        sent: Arc<Mutex<Vec<HttpRequest>>>,
        status: u16,
        body: Value,
    }

    impl SharedMock {
        fn ok(body: Value) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                status: 200,
                body,
            }
        }
        fn last(&self) -> HttpRequest {
            self.sent
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("une requête")
        }
    }

    impl HttpClient for SharedMock {
        fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
            self.sent.lock().unwrap().push(req.clone());
            Ok(HttpResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    /// Résolveur de test : token fixe pour tout provider (ou aucun).
    struct MockResolver(Option<String>);
    impl CredentialResolver for MockResolver {
        fn token(&self, _provider: &str) -> Result<Option<String>, String> {
            Ok(self.0.clone())
        }
    }

    fn resolver(tok: &str) -> MockResolver {
        MockResolver(Some(tok.into()))
    }

    #[test]
    fn slack_send_message_injects_bearer_and_maps_response() {
        let http = SharedMock::ok(json!({ "ok": true, "ts": "1.2" }));
        let out = dispatch_action(
            "slack.send_message",
            &json!({ "channel": "C1", "text": "hi" }),
            &http,
            &resolver("xoxb-secret"),
        )
        .expect("dispatch ok");
        // Auth injectée dans la requête réellement envoyée.
        assert_eq!(
            http.last().headers.get("Authorization").map(String::as_str),
            Some("Bearer xoxb-secret")
        );
        // Réponse du connecteur remontée.
        assert_eq!(out["ok"], json!(true));
    }

    #[test]
    fn each_provider_injects_its_token() {
        for (action, body) in [
            ("gmail.send_email", json!({ "id": "m1" })),
            ("notion.create_page", json!({ "id": "p1" })),
            ("github.create_issue", json!({ "number": 1 })),
        ] {
            let http = SharedMock::ok(body);
            let params = match action {
                "gmail.send_email" => json!({ "to": "a@b.c", "subject": "s", "body": "b" }),
                "notion.create_page" => {
                    json!({ "parent": { "database_id": "db1" }, "properties": { "Name": "t" } })
                }
                _ => json!({ "owner": "o", "repo": "r", "title": "t" }),
            };
            let res = dispatch_action(action, &params, &http, &resolver("tok-123"));
            assert!(res.is_ok(), "{action} devrait réussir : {res:?}");
            assert_eq!(
                http.last().headers.get("Authorization").map(String::as_str),
                Some("Bearer tok-123"),
                "{action} doit injecter le bearer"
            );
        }
    }

    #[test]
    fn missing_token_is_an_error_naming_the_env_var() {
        let http = SharedMock::ok(json!({}));
        let err = dispatch_action(
            "slack.send_message",
            &json!({ "channel": "C1", "text": "x" }),
            &http,
            &MockResolver(None),
        )
        .unwrap_err();
        assert!(err.contains("AUTOME_CONNECTOR_TOKEN_SLACK"), "msg: {err}");
    }

    #[test]
    fn unknown_action_is_an_error() {
        let http = SharedMock::ok(json!({}));
        let err = dispatch_action("dropbox.upload", &json!({}), &http, &resolver("t")).unwrap_err();
        assert!(err.contains("non supportée"), "msg: {err}");
    }

    #[test]
    fn is_connector_action_recognizes_known_providers() {
        assert!(is_connector_action("slack.send_message"));
        assert!(is_connector_action("github.merge_pull"));
        assert!(!is_connector_action("http_request"));
        assert!(!is_connector_action("dropbox.upload"));
    }

    #[test]
    fn env_resolver_reads_provider_var() {
        // Variable dédiée et improbable pour éviter les collisions de test.
        let var = "AUTOME_CONNECTOR_TOKEN_SLACK";
        let prev = std::env::var(var).ok();
        std::env::set_var(var, "xoxb-from-env");
        let got = EnvCredentialResolver.token("slack").unwrap();
        assert_eq!(got.as_deref(), Some("xoxb-from-env"));
        assert_eq!(EnvCredentialResolver.token("notconfigured").unwrap(), None);
        match prev {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }
}

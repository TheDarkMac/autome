//! Tests d'intégration **black-box** des connecteurs (F9.2).
//!
//! Contrairement aux tests inline (white-box, un fichier par connecteur),
//! ceux-ci pilotent les connecteurs via leur **seule API publique**
//! (`autome_core::Action`) contre un transport HTTP **mocké**, et vérifient
//! des invariants *transverses* :
//!
//! - mapping d'erreur uniforme (HTTP non-2xx → [`Error::Invalid`]) ;
//! - injection systématique de l'en-tête `Authorization` ;
//! - composition multi-étapes (sortie d'un connecteur → entrée d'un autre) ;
//! - intégration du `RetryingClient` du SDK avec un connecteur réel.
//!
//! Une sonde [`Probe`] (transport partagé via `Rc`) permet d'inspecter les
//! requêtes émises *après* que le client a été déplacé dans l'action — ce que
//! le `RecordingClient` (déplacé par valeur) ne permet pas depuis l'extérieur.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use autome_connectors::http::{HttpClient, HttpRequest, HttpResponse};
use autome_connectors::sdk::{Clock, RecordingClient, RetryPolicy, RetryingClient};
use autome_connectors::{
    GithubAuth, GithubCreateIssue, GmailAuth, GmailSendEmail, NotionAuth, NotionCreatePage,
    SlackAuth, SlackSendMessage,
};
use autome_core::{Action, Error};
use serde_json::{json, Value};

// ───────────────────────── sonde de transport partagée ─────────────────────────

/// Transport factice **clonable** : la sonde garde un handle (`Rc`) sur le
/// journal des requêtes pendant que l'action en détient un clone.
#[derive(Clone)]
struct Probe {
    sent: Rc<RefCell<Vec<HttpRequest>>>,
    response: Rc<std::result::Result<HttpResponse, String>>,
}

impl Probe {
    fn with_status(status: u16, body: Value) -> Self {
        Self {
            sent: Rc::new(RefCell::new(Vec::new())),
            response: Rc::new(Ok(HttpResponse { status, body })),
        }
    }
    fn ok(body: Value) -> Self {
        Self::with_status(200, body)
    }
    fn last(&self) -> Option<HttpRequest> {
        self.sent.borrow().last().cloned()
    }
}

impl HttpClient for Probe {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
        self.sent.borrow_mut().push(req.clone());
        (*self.response).clone()
    }
}

/// Horloge de test qui ne dort jamais (retry instantané).
struct NoSleep;
impl Clock for NoSleep {
    fn sleep(&self, _: Duration) {}
}

/// Params valides minimaux pour l'action « envoi » de chaque connecteur.
fn happy_params(provider: &str) -> Value {
    match provider {
        "slack" => json!({ "channel": "C1", "text": "hi" }),
        "gmail" => json!({ "to": "a@b.io", "subject": "Hi", "body": "Hello" }),
        "notion" => json!({ "parent": { "page_id": "p" }, "properties": {} }),
        "github" => json!({ "owner": "o", "repo": "r", "title": "Bug" }),
        other => panic!("provider inconnu : {other}"),
    }
}

/// Construit l'action « envoi » de chaque connecteur, adossée à `client`.
fn send_action<C: HttpClient + 'static>(provider: &str, client: C) -> Box<dyn Action> {
    match provider {
        "slack" => Box::new(SlackSendMessage::new(
            SlackAuth::Bot("xoxb-t".into()),
            client,
        )),
        "gmail" => Box::new(GmailSendEmail::new(GmailAuth::OAuth2("tok".into()), client)),
        "notion" => Box::new(NotionCreatePage::new(
            NotionAuth::Integration("secret".into()),
            client,
        )),
        "github" => Box::new(GithubCreateIssue::new(
            GithubAuth::Token("ghp".into()),
            client,
        )),
        other => panic!("provider inconnu : {other}"),
    }
}

const PROVIDERS: [&str; 4] = ["slack", "gmail", "notion", "github"];

// ───────────────────────────── invariants transverses ─────────────────────────────

/// Tout connecteur traduit une réponse HTTP non-2xx en `Error::Invalid`
/// (jamais de panique, jamais un autre variant).
#[test]
fn every_connector_maps_http_error_to_invalid() {
    for provider in PROVIDERS {
        let probe = Probe::with_status(502, json!({ "message": "bad gateway" }));
        let action = send_action(provider, probe);
        let err = action
            .execute(&happy_params(provider))
            .expect_err(&format!("{provider} : 502 doit échouer"));
        assert!(
            matches!(err, Error::Invalid(_)),
            "{provider} : attendu Error::Invalid, obtenu {err:?}"
        );
    }
}

/// Tout connecteur échoue proprement (`Error::Invalid`) sur erreur de transport.
#[test]
fn every_connector_maps_transport_error_to_invalid() {
    for provider in PROVIDERS {
        let client = RecordingClient::failing("connexion refusée");
        let action = send_action(provider, client);
        let err = action
            .execute(&happy_params(provider))
            .expect_err(&format!("{provider} : transport KO doit échouer"));
        assert!(matches!(err, Error::Invalid(_)), "{provider} : {err:?}");
    }
}

/// Tout connecteur émet un en-tête `Authorization` sur sa requête.
#[test]
fn every_connector_injects_authorization_header() {
    // Réponse « succès » acceptée par chaque interpréteur (Slack exige `ok`).
    let success = |provider: &str| match provider {
        "slack" => json!({ "ok": true, "ts": "1.0" }),
        _ => json!({ "id": "x", "number": 1 }),
    };
    for provider in PROVIDERS {
        let probe = Probe::ok(success(provider));
        let action = send_action(provider, probe.clone());
        action
            .execute(&happy_params(provider))
            .unwrap_or_else(|e| panic!("{provider} : envoi nominal a échoué : {e:?}"));
        let req = probe.last().expect("une requête émise");
        assert!(
            req.headers
                .get("Authorization")
                .is_some_and(|h| !h.is_empty()),
            "{provider} : en-tête Authorization manquant ({:?})",
            req.headers
        );
    }
}

// ───────────────────────────── composition multi-étapes ─────────────────────────────

/// La sortie d'un connecteur alimente l'entrée du suivant : un workflow type
/// « GitHub crée une issue → Slack notifie » fait transiter `number`/`html_url`
/// de la réponse GitHub vers la requête Slack.
#[test]
fn github_output_feeds_slack_notification() {
    let gh_probe = Probe::ok(json!({
        "number": 42,
        "html_url": "https://github.com/o/r/issues/42",
    }));
    let github = GithubCreateIssue::new(GithubAuth::Token("ghp".into()), gh_probe.clone());
    let issue = github
        .execute(&json!({ "owner": "o", "repo": "r", "title": "Bug" }))
        .expect("création issue");

    let number = issue["number"].as_i64().expect("number");
    let url = issue["html_url"].as_str().expect("html_url").to_string();

    let slack_probe = Probe::ok(json!({ "ok": true, "ts": "1.0" }));
    let slack = SlackSendMessage::new(SlackAuth::Bot("xoxb".into()), slack_probe.clone());
    slack
        .execute(&json!({
            "channel": "C1",
            "text": format!("Issue #{number} créée : {url}"),
        }))
        .expect("notification slack");

    // GitHub a bien reçu un POST .../issues.
    let gh_req = gh_probe.last().unwrap();
    assert_eq!(gh_req.method, "POST");
    assert!(gh_req.url.ends_with("/issues"), "url={}", gh_req.url);

    // Le message Slack porte les données issues de GitHub.
    let slack_text = slack_probe.last().unwrap().body.unwrap()["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(slack_text.contains("#42"), "texte={slack_text}");
    assert!(slack_text.contains("issues/42"), "texte={slack_text}");
}

// ───────────────────────────── intégration retry SDK ─────────────────────────────

/// Un transport instable (502 puis 200) enveloppé d'un `RetryingClient` rend
/// le retry **transparent** pour le connecteur : l'action réussit et ne voit
/// que la réponse finale.
#[test]
fn retrying_client_makes_transient_5xx_transparent_to_connector() {
    let inner = RecordingClient::queue(vec![
        Ok(HttpResponse {
            status: 502,
            body: json!({ "message": "bad gateway" }),
        }),
        Ok(HttpResponse {
            status: 200,
            body: json!({ "ok": true, "ts": "1.0" }),
        }),
    ]);
    let policy = RetryPolicy {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(0),
        backoff_multiplier: 2.0,
        max_backoff: Duration::from_millis(0),
        retry_on_status: vec![502],
    };
    let client = RetryingClient::new(inner, policy, NoSleep);
    let action = SlackSendMessage::new(SlackAuth::Bot("xoxb".into()), client);

    let out = action
        .execute(&json!({ "channel": "C1", "text": "hi" }))
        .expect("le retry doit absorber le 502 transitoire");
    assert_eq!(out["ok"], json!(true));
}

/// Si toutes les tentatives échouent (502 persistant), le connecteur remonte
/// bien un `Error::Invalid` (échec après épuisement des essais).
#[test]
fn retrying_client_surfaces_persistent_5xx_as_invalid() {
    let inner = RecordingClient::queue(vec![
        Ok(HttpResponse {
            status: 502,
            body: json!({}),
        }),
        Ok(HttpResponse {
            status: 502,
            body: json!({}),
        }),
    ]);
    let policy = RetryPolicy {
        max_attempts: 2,
        initial_backoff: Duration::from_millis(0),
        backoff_multiplier: 2.0,
        max_backoff: Duration::from_millis(0),
        retry_on_status: vec![502],
    };
    let client = RetryingClient::new(inner, policy, NoSleep);
    let action = SlackSendMessage::new(SlackAuth::Bot("xoxb".into()), client);

    let err = action
        .execute(&json!({ "channel": "C1", "text": "hi" }))
        .expect_err("502 persistant doit échouer");
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
}

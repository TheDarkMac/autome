//! Exemple de workflow **Webhook GitHub → notification Slack** (F12.2).
//!
//! ```text
//! entry (trigger webhook GitHub, événement `issues`)
//!   → slack.send_message  (poste un résumé de l'issue dans un canal)
//! ```
//!
//! Le test reproduit le chemin réel d'un webhook entrant :
//! 1. réception d'un payload GitHub `issues/opened` (octets bruts) ;
//! 2. **authentification** via `X-Hub-Signature-256` (`verify_github_signature`,
//!    F4.4.d) — l'ingress rejette tout payload non signé/altéré ;
//! 3. exécution du workflow par le **vrai moteur** : le node trigger fournit le
//!    payload parsé, le node `slack.send_message` (connecteur réel) en dérive un
//!    message et le poste — sur un transport HTTP factice (aucun réseau).
//!
//! Le graphe est chargé depuis `examples/workflows/github-webhook-to-slack.json`.

use std::sync::{Arc, Mutex};

use autome_connectors::{
    verify_github_signature, HttpClient, HttpRequest, HttpResponse, SlackAuth, SlackSendMessage,
};
use autome_core::{Action, NodeId, RunStatus, WorkflowGraph};
use autome_engine::{Dag, Engine, RunContext, StepStatus};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

/// Transport HTTP factice `Sync` (cf. exemple F12.1) : enregistre les requêtes
/// et rejoue une réponse fixe.
#[derive(Clone)]
struct MockHttp {
    sent: Arc<Mutex<Vec<HttpRequest>>>,
    response: Arc<HttpResponse>,
}

impl MockHttp {
    fn ok(body: Value) -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(HttpResponse { status: 200, body }),
        }
    }

    fn last(&self) -> HttpRequest {
        self.sent
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("une requête envoyée")
    }
}

impl HttpClient for MockHttp {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
        self.sent.lock().unwrap().push(req.clone());
        Ok((*self.response).clone())
    }
}

fn load_workflow() -> WorkflowGraph {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/workflows/github-webhook-to-slack.json"
    );
    let raw = std::fs::read_to_string(path).expect("lecture du JSON d'exemple");
    serde_json::from_str(&raw).expect("désérialisation WorkflowGraph")
}

/// Calcule l'en-tête `X-Hub-Signature-256` que GitHub envoie pour `body`.
fn github_signature(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("clé HMAC");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[test]
fn github_webhook_authenticates_then_notifies_slack() {
    // ── Payload GitHub `issues/opened` (tel que livré par le webhook) ──────
    let payload = json!({
        "action": "opened",
        "issue": {
            "number": 42,
            "title": "Bug: crash au démarrage",
            "html_url": "https://github.com/acme/app/issues/42",
            "user": { "login": "alice" }
        },
        "repository": { "full_name": "acme/app" },
        "sender": { "login": "alice" }
    });
    let body = serde_json::to_vec(&payload).expect("sérialisation payload");
    let secret = b"webhook-shared-secret";

    // ── Authentification de l'ingress (F4.4.d) ────────────────────────────
    let signature = github_signature(secret, &body);
    assert!(
        verify_github_signature(secret, &body, &signature),
        "la signature légitime doit être acceptée"
    );
    // Un payload altéré (même signature) est rejeté.
    let mut tampered = body.clone();
    tampered.extend_from_slice(b" ");
    assert!(
        !verify_github_signature(secret, &tampered, &signature),
        "un payload altéré doit être rejeté"
    );

    // ── Workflow ──────────────────────────────────────────────────────────
    let workflow = load_workflow();
    let dag = Dag::from_workflow(&workflow).expect("DAG valide");
    let slack =
        MockHttp::ok(json!({ "ok": true, "channel": "C123ISSUES", "ts": "1700000000.000200" }));

    let runner = {
        let slack = slack.clone();
        let event = payload.clone();
        move |node: &autome_core::Node, ctx: &RunContext| -> Result<Value, String> {
            match &node.kind {
                // Le trigger webhook fournit le payload parsé au reste du graphe
                // (ce que l'ingress HTTP injectera en production).
                autome_core::NodeKind::Trigger { .. } => Ok(event.clone()),
                autome_core::NodeKind::Action { action_type }
                    if action_type == "slack.send_message" =>
                {
                    let evt = ctx
                        .output(&NodeId::new("entry"))
                        .ok_or("slack : payload du webhook absent en amont")?;
                    let num = evt
                        .pointer("/issue/number")
                        .and_then(Value::as_i64)
                        .ok_or("issue.number")?;
                    let title = evt
                        .pointer("/issue/title")
                        .and_then(Value::as_str)
                        .ok_or("issue.title")?;
                    let url = evt
                        .pointer("/issue/html_url")
                        .and_then(Value::as_str)
                        .ok_or("issue.html_url")?;
                    let repo = evt
                        .pointer("/repository/full_name")
                        .and_then(Value::as_str)
                        .ok_or("repo")?;
                    let user = evt
                        .pointer("/sender/login")
                        .and_then(Value::as_str)
                        .ok_or("sender")?;
                    let text =
                        format!("🐛 [{repo}] Issue #{num} ouverte par {user} : {title}\n{url}");

                    // Fusionne le `channel` statique du node avec le texte calculé.
                    let mut params = node.params.as_object().cloned().unwrap_or_default();
                    params.insert("text".into(), json!(text));

                    SlackSendMessage::new(SlackAuth::Bot("xoxb-test".into()), slack.clone())
                        .execute(&Value::Object(params))
                        .map_err(|e| e.to_string())
                }
                autome_core::NodeKind::Action { action_type } => {
                    Err(format!("action non gérée par l'exemple : {action_type}"))
                }
                _ => Ok(Value::Null),
            }
        }
    };

    let report = Engine::new().run(&dag, &runner);

    // ── Le run aboutit, les 2 nodes réussissent ───────────────────────────
    assert_eq!(report.status, RunStatus::Succeeded, "run KO : {report:?}");
    for id in ["entry", "notify"] {
        let step = report
            .step(&NodeId::new(id))
            .unwrap_or_else(|| panic!("step {id} absent"));
        assert_eq!(
            step.status,
            StepStatus::Succeeded,
            "step {id} KO : {step:?}"
        );
    }

    // ── Slack : POST chat.postMessage, bon canal, texte dérivé du payload ──
    let s = slack.last();
    assert_eq!(s.method, "POST");
    assert!(
        s.url.ends_with("/chat.postMessage"),
        "url slack : {}",
        s.url
    );
    let sent_body = s.body.as_ref().expect("corps slack");
    assert_eq!(
        sent_body.get("channel").and_then(Value::as_str),
        Some("C123ISSUES")
    );
    let text = sent_body.get("text").and_then(Value::as_str).expect("text");
    assert!(text.contains("Issue #42"), "texte slack : {text}");
    assert!(
        text.contains("Bug: crash au démarrage"),
        "texte slack : {text}"
    );
    assert!(text.contains("acme/app"), "texte slack : {text}");
}

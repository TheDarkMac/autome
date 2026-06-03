//! Exemple de workflow **Slack → Gmail → Notion** (F12.1).
//!
//! Démontre, de bout en bout et **sans réseau**, un automatisme réaliste :
//!
//! ```text
//! entry (trigger manuel)
//!   → slack.read_channel   (lit le dernier message du canal #deploys)
//!   → gmail.send_email      (envoie un résumé reprenant ce message)
//!   → notion.create_page    (archive message + id de l'email dans Notion)
//! ```
//!
//! Le graphe est **chargé depuis** `examples/workflows/slack-to-gmail-notion.json`
//! (preuve qu'un `WorkflowGraph` est un artefact déclaratif désérialisable),
//! puis exécuté par le **vrai moteur** (`Engine::run`) avec les **vrais
//! connecteurs** (`SlackReadChannel`, `GmailSendEmail`, `NotionCreatePage`)
//! branchés sur un transport HTTP factice. Le `NodeRunner` joue le rôle de la
//! glue de mapping de données (F2.6) entre les nodes.

use std::sync::{Arc, Mutex};

use autome_connectors::{
    GmailAuth, GmailSendEmail, HttpClient, HttpRequest, HttpResponse, NotionAuth, NotionCreatePage,
    SlackAuth, SlackReadChannel,
};
use autome_core::{Action, NodeId, RunStatus, WorkflowGraph};
use autome_engine::{Dag, Engine, RunContext, StepStatus};
use base64::Engine as _;
use serde_json::{json, Value};

/// Transport HTTP factice **`Sync`** (le moteur exige `NodeRunner + Sync`, donc
/// pas le `RecordingClient` à base de `RefCell` du SDK). Cloneable avec état
/// partagé : on en passe une copie au connecteur et on inspecte l'original.
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

    fn count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

impl HttpClient for MockHttp {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
        self.sent.lock().unwrap().push(req.clone());
        Ok((*self.response).clone())
    }
}

/// Fusionne l'objet `params` statique du node avec des champs calculés
/// (mapping de données issu des sorties amont).
fn with_params(base: &Value, extra: Vec<(&str, Value)>) -> Value {
    let mut obj = base.as_object().cloned().unwrap_or_default();
    for (k, v) in extra {
        obj.insert(k.to_string(), v);
    }
    Value::Object(obj)
}

fn load_workflow() -> WorkflowGraph {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/workflows/slack-to-gmail-notion.json"
    );
    let raw = std::fs::read_to_string(path).expect("lecture du JSON d'exemple");
    serde_json::from_str(&raw).expect("désérialisation WorkflowGraph")
}

#[test]
fn slack_to_gmail_to_notion_runs_end_to_end() {
    let workflow = load_workflow();
    let dag = Dag::from_workflow(&workflow).expect("DAG valide");

    // Réponses scriptées des trois services.
    let slack_text = "Déploiement v1.2.3 terminé ✅";
    let slack = MockHttp::ok(json!({
        "ok": true,
        "messages": [
            { "type": "message", "user": "U_DEPLOYBOT", "text": slack_text, "ts": "1700000000.000100" }
        ]
    }));
    let gmail =
        MockHttp::ok(json!({ "id": "gmail-msg-1", "threadId": "t-1", "labelIds": ["SENT"] }));
    let notion = MockHttp::ok(
        json!({ "object": "page", "id": "notion-page-1", "url": "https://notion.so/notion-page-1" }),
    );

    // Le runner câble chaque node sur le connecteur réel et fait transiter la
    // donnée d'un node à l'autre (ce que produira le mapping F2.6 en prod).
    let runner = {
        let (slack, gmail, notion) = (slack.clone(), gmail.clone(), notion.clone());
        move |node: &autome_core::Node, ctx: &RunContext| -> Result<Value, String> {
            let action_type = match &node.kind {
                autome_core::NodeKind::Action { action_type } => action_type.as_str(),
                // Le trigger n'exécute aucun appel : il amorce simplement le run.
                _ => return Ok(Value::Null),
            };
            match action_type {
                "slack.read_channel" => {
                    SlackReadChannel::new(SlackAuth::Bot("xoxb-test".into()), slack.clone())
                        .execute(&node.params)
                        .map_err(|e| e.to_string())
                }

                "gmail.send_email" => {
                    // Mapping : le corps de l'email = dernier message Slack.
                    let text = ctx
                        .output(&NodeId::new("slack_read"))
                        .and_then(|o| o.pointer("/messages/0/text"))
                        .and_then(Value::as_str)
                        .ok_or("gmail : message Slack introuvable en amont")?;
                    let params = with_params(
                        &node.params,
                        vec![("body", json!(format!("Dernier message #deploys : {text}")))],
                    );
                    GmailSendEmail::new(GmailAuth::OAuth2("ya29.test".into()), gmail.clone())
                        .execute(&params)
                        .map_err(|e| e.to_string())
                }

                "notion.create_page" => {
                    // Mapping : titre = message Slack ; on trace l'id de l'email.
                    let text = ctx
                        .output(&NodeId::new("slack_read"))
                        .and_then(|o| o.pointer("/messages/0/text"))
                        .and_then(Value::as_str)
                        .ok_or("notion : message Slack introuvable en amont")?;
                    let email_id = ctx
                        .output(&NodeId::new("gmail_send"))
                        .and_then(|o| o.get("id"))
                        .and_then(Value::as_str)
                        .ok_or("notion : id de l'email introuvable en amont")?;
                    let properties = json!({
                        "Name": { "title": [ { "text": { "content": text } } ] },
                        "EmailId": { "rich_text": [ { "text": { "content": email_id } } ] }
                    });
                    let params = with_params(&node.params, vec![("properties", properties)]);
                    NotionCreatePage::new(
                        NotionAuth::Integration("secret_test".into()),
                        notion.clone(),
                    )
                    .execute(&params)
                    .map_err(|e| e.to_string())
                }

                other => Err(format!("action non gérée par l'exemple : {other}")),
            }
        }
    };

    let report = Engine::new().run(&dag, &runner);

    // ── Le run aboutit, les 4 nodes réussissent ───────────────────────────
    assert_eq!(report.status, RunStatus::Succeeded, "run KO : {report:?}");
    for id in ["entry", "slack_read", "gmail_send", "notion_create"] {
        let step = report
            .step(&NodeId::new(id))
            .unwrap_or_else(|| panic!("step {id} absent"));
        assert_eq!(
            step.status,
            StepStatus::Succeeded,
            "step {id} non réussi : {step:?}"
        );
    }

    // ── Slack : GET conversations.history sur le bon canal ─────────────────
    let s = slack.last();
    assert_eq!(s.method, "GET");
    assert!(
        s.url.contains("conversations.history"),
        "url slack : {}",
        s.url
    );
    assert!(
        s.url.contains("channel=C123DEPLOYS"),
        "canal slack : {}",
        s.url
    );

    // ── Gmail : POST messages/send ; le `raw` décodé contient le texte Slack ─
    assert_eq!(gmail.count(), 1);
    let g = gmail.last();
    assert_eq!(g.method, "POST");
    assert!(g.url.contains("/messages/send"), "url gmail : {}", g.url);
    let raw = g
        .body
        .as_ref()
        .and_then(|b| b.get("raw"))
        .and_then(Value::as_str)
        .expect("gmail raw");
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .expect("base64url gmail");
    let mime = String::from_utf8(decoded).expect("MIME utf-8");
    assert!(
        mime.contains("ops@example.com"),
        "destinataire absent du MIME"
    );
    assert!(
        mime.contains(slack_text),
        "le message Slack n'a pas été propagé dans l'email"
    );

    // ── Notion : POST pages ; titre = message Slack, et id de l'email tracé ─
    let n = notion.last();
    assert_eq!(n.method, "POST");
    assert!(n.url.ends_with("/pages"), "url notion : {}", n.url);
    let body = n.body.as_ref().expect("notion body");
    assert_eq!(
        body.pointer("/parent/database_id").and_then(Value::as_str),
        Some("db-archive-0001")
    );
    assert_eq!(
        body.pointer("/properties/Name/title/0/text/content")
            .and_then(Value::as_str),
        Some(slack_text),
        "le titre Notion devrait reprendre le message Slack"
    );
    assert_eq!(
        body.pointer("/properties/EmailId/rich_text/0/text/content")
            .and_then(Value::as_str),
        Some("gmail-msg-1"),
        "l'id de l'email Gmail devrait être archivé dans Notion"
    );
}

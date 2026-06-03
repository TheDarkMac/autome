//! Exemple de workflow **Scheduler → fetch API → stockage** (F12.3).
//!
//! ```text
//! entry (trigger schedule, cron "0 * * * *")
//!   → http_request GET   (récupère la météo depuis une API)
//!   → http_request POST  (stocke l'enregistrement dérivé)
//! ```
//!
//! Le test :
//! 1. **gate scheduler** : `CronSchedule::parse` + `matches(CronTime)` — le run
//!    n'a lieu qu'à l'heure pile (primitive F3.3/F5.3) ;
//! 2. exécute le workflow par le **vrai moteur** avec le connecteur HTTP réel
//!    (`HttpAction`) sur un transport factice (un client par node) ;
//! 3. **mappe** la réponse du fetch vers le corps du POST de stockage.
//!
//! Graphe chargé depuis `examples/workflows/scheduler-fetch-store.json`.

use std::sync::{Arc, Mutex};

use autome_connectors::{
    CronSchedule, CronTime, HttpAction, HttpClient, HttpRequest, HttpResponse,
};
use autome_core::{Action, NodeId, RunStatus, WorkflowGraph};
use autome_engine::{Dag, Engine, RunContext, StepStatus};
use serde_json::{json, Value};

/// Transport HTTP factice `Sync` (cf. exemples F12.1/F12.2).
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
        "/../../examples/workflows/scheduler-fetch-store.json"
    );
    let raw = std::fs::read_to_string(path).expect("lecture du JSON d'exemple");
    serde_json::from_str(&raw).expect("désérialisation WorkflowGraph")
}

#[test]
fn scheduler_gate_then_fetch_then_store() {
    let workflow = load_workflow();

    // ── Gate scheduler : le cron du node `entry` détermine si le run est dû ──
    let cron_expr = workflow
        .node(&NodeId::new("entry"))
        .and_then(|n| n.params.get("cron"))
        .and_then(Value::as_str)
        .expect("cron du trigger");
    let schedule = CronSchedule::parse(cron_expr).expect("cron valide");
    let due = CronTime {
        minute: 0,
        hour: 9,
        day: 1,
        month: 6,
        weekday: 1,
    };
    let not_due = CronTime { minute: 30, ..due };
    assert!(schedule.matches(&due), "le run doit être dû à HH:00");
    assert!(!schedule.matches(&not_due), "pas de run à HH:30");

    // ── Le scheduler a déclenché : on exécute le workflow ──────────────────
    let dag = Dag::from_workflow(&workflow).expect("DAG valide");
    let fetch_http = MockHttp::ok(json!({ "city": "Paris", "temp_c": 21, "humidity": 62 }));
    let store_http = MockHttp::ok(json!({ "id": "rec-1", "stored": true }));

    let runner = {
        let (fetch_http, store_http) = (fetch_http.clone(), store_http.clone());
        move |node: &autome_core::Node, ctx: &RunContext| -> Result<Value, String> {
            match &node.kind {
                autome_core::NodeKind::Trigger { .. } => Ok(json!({ "fired": true })),
                autome_core::NodeKind::Action { action_type } if action_type == "http_request" => {
                    // Les deux nodes sont des `http_request` : on les distingue
                    // par leur identifiant (fetch en amont, store en aval).
                    match node.id.0.as_str() {
                        "fetch" => HttpAction::new(fetch_http.clone())
                            .execute(&node.params)
                            .map_err(|e| e.to_string()),
                        "store" => {
                            // Mapping : construire l'enregistrement à stocker à
                            // partir du corps renvoyé par le fetch.
                            let fetched = ctx
                                .output(&NodeId::new("fetch"))
                                .and_then(|o| o.get("body"))
                                .ok_or("store : sortie du fetch absente")?;
                            let city = fetched
                                .get("city")
                                .and_then(Value::as_str)
                                .ok_or("fetch.city")?;
                            let temp = fetched
                                .get("temp_c")
                                .and_then(Value::as_i64)
                                .ok_or("fetch.temp_c")?;
                            let record = json!({
                                "source": "weather-api",
                                "city": city,
                                "temperature_c": temp,
                            });
                            // Fusionne method/url statiques + body calculé.
                            let mut params = node.params.as_object().cloned().unwrap_or_default();
                            params.insert("body".into(), record);
                            HttpAction::new(store_http.clone())
                                .execute(&Value::Object(params))
                                .map_err(|e| e.to_string())
                        }
                        other => Err(format!("node http inattendu : {other}")),
                    }
                }
                autome_core::NodeKind::Action { action_type } => {
                    Err(format!("action non gérée : {action_type}"))
                }
                _ => Ok(Value::Null),
            }
        }
    };

    let report = Engine::new().run(&dag, &runner);

    // ── Le run aboutit, les 3 nodes réussissent ───────────────────────────
    assert_eq!(report.status, RunStatus::Succeeded, "run KO : {report:?}");
    for id in ["entry", "fetch", "store"] {
        let step = report
            .step(&NodeId::new(id))
            .unwrap_or_else(|| panic!("step {id} absent"));
        assert_eq!(
            step.status,
            StepStatus::Succeeded,
            "step {id} KO : {step:?}"
        );
    }

    // ── Fetch : GET sur l'API météo ───────────────────────────────────────
    let f = fetch_http.last();
    assert_eq!(f.method, "GET");
    assert_eq!(f.url, "https://api.weather.example/v1/current?city=Paris");

    // ── Store : POST sur le service de stockage, enregistrement dérivé du fetch
    let s = store_http.last();
    assert_eq!(s.method, "POST");
    assert_eq!(s.url, "https://storage.example/v1/records");
    let body = s.body.as_ref().expect("corps du POST de stockage");
    assert_eq!(body.get("city").and_then(Value::as_str), Some("Paris"));
    assert_eq!(body.get("temperature_c").and_then(Value::as_i64), Some(21));
    assert_eq!(
        body.get("source").and_then(Value::as_str),
        Some("weather-api")
    );
}

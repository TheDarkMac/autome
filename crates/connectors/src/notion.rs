//! Connecteur Notion (F4.3, refactoré sous le SDK en F4.5).

use std::collections::BTreeMap;

use autome_core::{Action, Error, Result};
use serde_json::{json, Value};

use crate::http::{HttpClient, HttpRequest};
use crate::sdk::{
    headers_for, interpret, percent_encode, ActionSchema, Connector, Credentials, ParamSpec,
    ResponseInterpreter,
};

const NOTION_API: &str = "https://api.notion.com/v1";

/// Version d'API Notion supportée (`Notion-Version`).
pub const NOTION_VERSION: &str = "2022-06-28";

/// Stratégie d'authentification Notion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotionAuth {
    /// Token d'intégration interne (`secret_…`).
    Integration(String),
}

impl Credentials for NotionAuth {
    fn apply(&self, headers: &mut BTreeMap<String, String>) {
        match self {
            NotionAuth::Integration(t) => {
                headers.insert("Authorization".into(), format!("Bearer {t}"));
            }
        }
        headers.insert("Notion-Version".into(), NOTION_VERSION.into());
    }
}

/// Interpréteur Notion (extrait `code`/`message` du body d'erreur).
pub struct NotionInterpreter;

impl ResponseInterpreter for NotionInterpreter {
    fn provider(&self) -> &'static str {
        "notion"
    }
    fn extract_error(&self, body: &Value) -> Option<String> {
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        let code = body.get("code").and_then(Value::as_str).map(str::to_string);
        match (message, code) {
            (Some(m), Some(c)) => Some(format!("{m} ({c})")),
            (Some(m), None) => Some(m),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        }
    }
}

/// Connecteur Notion.
pub struct NotionConnector;

impl Connector for NotionConnector {
    fn provider(&self) -> &'static str {
        "notion"
    }
    fn action_schemas(&self) -> Vec<ActionSchema> {
        vec![
            create_page_schema(),
            read_page_schema(),
            query_database_schema(),
        ]
    }
}

// ──────────── Helpers blocs (F4.3.d) ────────────

/// Construit un block Notion `paragraph` à partir d'une chaîne plate.
pub fn text_block(text: impl Into<String>) -> Value {
    let text = text.into();
    json!({
        "object": "block",
        "type": "paragraph",
        "paragraph": {
            "rich_text": [
                { "type": "text", "text": { "content": text } }
            ]
        }
    })
}

/// Construit un block `heading_<level>` (niveau clampé à `1..=3`).
pub fn heading_block(text: impl Into<String>, level: u8) -> Value {
    let lvl = level.clamp(1, 3);
    let key = match lvl {
        1 => "heading_1",
        2 => "heading_2",
        _ => "heading_3",
    };
    let text = text.into();
    json!({
        "object": "block",
        "type": key,
        key: {
            "rich_text": [
                { "type": "text", "text": { "content": text } }
            ]
        }
    })
}

/// Extrait récursivement tous les `plain_text` sous `value`.
pub fn extract_plain_text(value: &Value) -> String {
    let mut out = String::new();
    walk(value, &mut out);
    out
}

fn walk(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            if let Some(s) = map.get("plain_text").and_then(Value::as_str) {
                out.push_str(s);
            }
            for (_, child) in map {
                if !matches!(
                    child,
                    Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null
                ) {
                    walk(child, out);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr {
                walk(child, out);
            }
        }
        _ => {}
    }
}

// ───────────────────────── Actions ─────────────────────────

/// Action `POST /v1/pages` (F4.3.b).
pub struct NotionCreatePage<C: HttpClient> {
    auth: NotionAuth,
    client: C,
}

impl<C: HttpClient> NotionCreatePage<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: NotionAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let parent = params
            .get("parent")
            .ok_or_else(|| Error::Invalid("notion create_page : `parent` requis".into()))?;
        let properties = params
            .get("properties")
            .ok_or_else(|| Error::Invalid("notion create_page : `properties` requis".into()))?;
        let mut body = serde_json::Map::new();
        body.insert("parent".into(), parent.clone());
        body.insert("properties".into(), properties.clone());
        for key in ["children", "icon", "cover"] {
            if let Some(v) = params.get(key) {
                body.insert(key.into(), v.clone());
            }
        }
        Ok(HttpRequest {
            method: "POST".into(),
            url: format!("{NOTION_API}/pages"),
            headers: headers_for(&self.auth, true),
            body: Some(Value::Object(body)),
        })
    }
}

impl<C: HttpClient> Action for NotionCreatePage<C> {
    fn kind(&self) -> &'static str {
        "notion.create_page"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("notion create_page : {e}")))?;
        interpret(resp, &NotionInterpreter)
    }
}

/// Action `GET /v1/pages/{id}` (F4.3.c).
pub struct NotionReadPage<C: HttpClient> {
    auth: NotionAuth,
    client: C,
}

impl<C: HttpClient> NotionReadPage<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: NotionAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("notion read_page : `id` requis".into()))?;
        Ok(HttpRequest {
            method: "GET".into(),
            url: format!("{NOTION_API}/pages/{}", percent_encode(id)),
            headers: headers_for(&self.auth, false),
            body: None,
        })
    }
}

impl<C: HttpClient> Action for NotionReadPage<C> {
    fn kind(&self) -> &'static str {
        "notion.read_page"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("notion read_page : {e}")))?;
        interpret(resp, &NotionInterpreter)
    }
}

/// Action `POST /v1/databases/{id}/query` (F4.3.c).
pub struct NotionQueryDatabase<C: HttpClient> {
    auth: NotionAuth,
    client: C,
}

impl<C: HttpClient> NotionQueryDatabase<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: NotionAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let database_id = params
            .get("database_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("notion query_database : `database_id` requis".into()))?;
        let mut body = serde_json::Map::new();
        for key in ["filter", "sorts", "page_size", "start_cursor"] {
            if let Some(v) = params.get(key) {
                body.insert(key.into(), v.clone());
            }
        }
        Ok(HttpRequest {
            method: "POST".into(),
            url: format!(
                "{NOTION_API}/databases/{}/query",
                percent_encode(database_id)
            ),
            headers: headers_for(&self.auth, true),
            body: Some(Value::Object(body)),
        })
    }
}

impl<C: HttpClient> Action for NotionQueryDatabase<C> {
    fn kind(&self) -> &'static str {
        "notion.query_database"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("notion query_database : {e}")))?;
        interpret(resp, &NotionInterpreter)
    }
}

// ───────────────────────── Schémas ─────────────────────────

/// Schéma de l'action `notion.create_page`.
pub fn create_page_schema() -> ActionSchema {
    ActionSchema {
        provider: "notion",
        kind: "notion.create_page",
        label: "Notion — Créer une page",
        description: "Crée une page via `POST /v1/pages`.",
        params: vec![
            ParamSpec::opt_obj("parent", "Parent"),
            ParamSpec::opt_obj("properties", "Properties"),
            ParamSpec::opt_obj("children", "Children (blocs)"),
            ParamSpec::opt_obj("icon", "Icon"),
            ParamSpec::opt_obj("cover", "Cover"),
        ],
    }
}

/// Schéma de l'action `notion.read_page`.
pub fn read_page_schema() -> ActionSchema {
    ActionSchema {
        provider: "notion",
        kind: "notion.read_page",
        label: "Notion — Lire une page",
        description: "Lit une page via `GET /v1/pages/{id}`.",
        params: vec![ParamSpec::req_str("id", "ID de page")],
    }
}

/// Schéma de l'action `notion.query_database`.
pub fn query_database_schema() -> ActionSchema {
    ActionSchema {
        provider: "notion",
        kind: "notion.query_database",
        label: "Notion — Query database",
        description: "Interroge une base via `POST /v1/databases/{id}/query`.",
        params: vec![
            ParamSpec::req_str("database_id", "Database ID"),
            ParamSpec::opt_obj("filter", "Filter"),
            ParamSpec::opt_obj("sorts", "Sorts"),
            ParamSpec::opt_int("page_size", "Page size"),
            ParamSpec::opt_str("start_cursor", "Start cursor"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::RecordingClient;
    use serde_json::json;

    #[test]
    fn all_actions_inject_bearer_and_notion_version() {
        let auth = NotionAuth::Integration("secret_abc".into());

        let create =
            NotionCreatePage::new(auth.clone(), RecordingClient::ok(json!({ "id": "p1" })));
        create
            .execute(&json!({
                "parent": { "database_id": "db" },
                "properties": {}
            }))
            .unwrap();
        let sent = create.client.last().unwrap();
        assert_eq!(
            sent.headers.get("Authorization").map(String::as_str),
            Some("Bearer secret_abc")
        );
        assert_eq!(
            sent.headers.get("Notion-Version").map(String::as_str),
            Some("2022-06-28")
        );

        let read = NotionReadPage::new(auth.clone(), RecordingClient::ok(json!({ "id": "p" })));
        read.execute(&json!({ "id": "p" })).unwrap();
        let sent = read.client.last().unwrap();
        assert_eq!(
            sent.headers.get("Authorization").map(String::as_str),
            Some("Bearer secret_abc")
        );
        assert_eq!(
            sent.headers.get("Notion-Version").map(String::as_str),
            Some("2022-06-28")
        );
        assert!(!sent.headers.contains_key("Content-Type"));

        let query = NotionQueryDatabase::new(
            auth,
            RecordingClient::ok(json!({ "results": [], "has_more": false })),
        );
        query.execute(&json!({ "database_id": "db" })).unwrap();
        let sent = query.client.last().unwrap();
        assert_eq!(
            sent.headers.get("Authorization").map(String::as_str),
            Some("Bearer secret_abc")
        );
        assert_eq!(
            sent.headers.get("Notion-Version").map(String::as_str),
            Some("2022-06-28")
        );
    }

    #[test]
    fn create_page_builds_post_with_required_and_optional_fields() {
        let act = NotionCreatePage::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::ok(json!({ "id": "p-new", "url": "https://notion.so/p-new" })),
        );
        let children = json!([{ "object": "block", "type": "paragraph",
                                "paragraph": { "rich_text": [] } }]);
        let out = act
            .execute(&json!({
                "parent": { "database_id": "db-1" },
                "properties": {
                    "Name": { "title": [ { "text": { "content": "Hello" } } ] }
                },
                "children": children,
                "icon": { "type": "emoji", "emoji": "✅" }
            }))
            .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.url, "https://api.notion.com/v1/pages");
        let body = sent.body.unwrap();
        assert_eq!(body["parent"]["database_id"], json!("db-1"));
        assert_eq!(
            body["properties"]["Name"]["title"][0]["text"]["content"],
            json!("Hello")
        );
        assert_eq!(body["children"], children);
        assert_eq!(body["icon"]["emoji"], json!("✅"));
        assert!(body.get("cover").is_none());
        assert_eq!(out["id"], json!("p-new"));
    }

    #[test]
    fn create_page_requires_parent_and_properties() {
        let act = NotionCreatePage::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        let err = act.execute(&json!({ "properties": {} })).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("`parent`")));

        let act = NotionCreatePage::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        let err = act
            .execute(&json!({ "parent": { "database_id": "db" } }))
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("`properties`")));
    }

    #[test]
    fn read_page_builds_get_with_percent_encoded_id() {
        let act = NotionReadPage::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::ok(json!({
                "object": "page",
                "id": "a/b",
                "properties": {}
            })),
        );
        let out = act.execute(&json!({ "id": "a/b" })).unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "GET");
        assert_eq!(sent.url, "https://api.notion.com/v1/pages/a%2Fb");
        assert!(sent.body.is_none());
        assert_eq!(out["id"], json!("a/b"));
    }

    #[test]
    fn read_page_requires_id() {
        let act = NotionReadPage::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        let err = act.execute(&json!({})).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("`id`")));
    }

    #[test]
    fn query_database_builds_post_with_filter_sorts_pagination() {
        let act = NotionQueryDatabase::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::ok(json!({ "results": [], "has_more": false })),
        );
        let filter = json!({ "property": "Status", "select": { "equals": "Done" } });
        let sorts = json!([{ "property": "Created", "direction": "descending" }]);
        act.execute(&json!({
            "database_id": "db-9",
            "filter": filter,
            "sorts": sorts,
            "page_size": 25,
            "start_cursor": "cur-1"
        }))
        .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.url, "https://api.notion.com/v1/databases/db-9/query");
        let body = sent.body.unwrap();
        assert_eq!(body["filter"], filter);
        assert_eq!(body["sorts"], sorts);
        assert_eq!(body["page_size"], json!(25));
        assert_eq!(body["start_cursor"], json!("cur-1"));
    }

    #[test]
    fn query_database_works_without_filter_or_sorts() {
        let act = NotionQueryDatabase::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::ok(json!({ "results": [], "has_more": false })),
        );
        act.execute(&json!({ "database_id": "db" })).unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.url, "https://api.notion.com/v1/databases/db/query");
        assert_eq!(sent.body.unwrap(), json!({}));
    }

    #[test]
    fn query_database_requires_database_id() {
        let act = NotionQueryDatabase::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::ok(json!({ "results": [] })),
        );
        let err = act.execute(&json!({})).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("`database_id`")));
    }

    // F4.3.d — helpers blocs

    #[test]
    fn text_block_produces_paragraph_with_rich_text() {
        let b = text_block("hello");
        assert_eq!(b["object"], json!("block"));
        assert_eq!(b["type"], json!("paragraph"));
        assert_eq!(b["paragraph"]["rich_text"][0]["type"], json!("text"));
        assert_eq!(
            b["paragraph"]["rich_text"][0]["text"]["content"],
            json!("hello")
        );
    }

    #[test]
    fn heading_block_supports_levels_1_to_3_and_clamps() {
        let h1 = heading_block("A", 1);
        assert_eq!(h1["type"], json!("heading_1"));
        assert_eq!(
            h1["heading_1"]["rich_text"][0]["text"]["content"],
            json!("A")
        );
        let h2 = heading_block("B", 2);
        assert_eq!(h2["type"], json!("heading_2"));
        let h3 = heading_block("C", 3);
        assert_eq!(h3["type"], json!("heading_3"));
        assert_eq!(heading_block("X", 0)["type"], json!("heading_1"));
        assert_eq!(heading_block("Y", 9)["type"], json!("heading_3"));
    }

    #[test]
    fn extract_plain_text_collects_rich_text_recursively() {
        let page = json!({
            "object": "page",
            "id": "p1",
            "properties": {
                "Name": {
                    "title": [
                        { "plain_text": "Hello " },
                        { "plain_text": "world" }
                    ]
                }
            }
        });
        let s = extract_plain_text(&page);
        assert_eq!(s, "Hello world");
    }

    #[test]
    fn extract_plain_text_walks_query_results() {
        let resp = json!({
            "object": "list",
            "results": [
                { "properties": { "Name": { "title": [ { "plain_text": "Row 1" } ] } } },
                { "properties": { "Name": { "title": [ { "plain_text": "Row 2" } ] } } }
            ],
            "has_more": false
        });
        let s = extract_plain_text(&resp);
        assert!(s.contains("Row 1"));
        assert!(s.contains("Row 2"));
    }

    #[test]
    fn extract_plain_text_returns_empty_when_no_rich_text() {
        assert_eq!(extract_plain_text(&json!({})), "");
        assert_eq!(extract_plain_text(&json!([1, "x", true])), "");
        assert_eq!(extract_plain_text(&Value::Null), "");
    }

    // Mapping erreurs

    #[test]
    fn notion_error_extracts_code_and_message() {
        let act = NotionReadPage::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::with_status(
                404,
                json!({
                    "object": "error",
                    "status": 404,
                    "code": "object_not_found",
                    "message": "Could not find page with ID"
                }),
            ),
        );
        let err = act.execute(&json!({ "id": "missing" })).unwrap_err();
        match err {
            Error::Invalid(m) => {
                assert!(m.contains("http 404"), "msg={m}");
                assert!(m.contains("Could not find page with ID"), "msg={m}");
                assert!(m.contains("object_not_found"), "msg={m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn notion_error_without_known_fields_falls_back_to_body() {
        let act = NotionCreatePage::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::with_status(500, json!({ "details": "boom" })),
        );
        let err = act
            .execute(&json!({ "parent": {}, "properties": {} }))
            .unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("http 500"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn transport_error_is_propagated() {
        let act = NotionCreatePage::new(
            NotionAuth::Integration("t".into()),
            RecordingClient::failing("network down"),
        );
        let err = act
            .execute(&json!({ "parent": {}, "properties": {} }))
            .unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("network down"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn kinds_are_stable() {
        assert_eq!(
            NotionCreatePage::new(
                NotionAuth::Integration("t".into()),
                RecordingClient::ok(json!({}))
            )
            .kind(),
            "notion.create_page"
        );
        assert_eq!(
            NotionReadPage::new(
                NotionAuth::Integration("t".into()),
                RecordingClient::ok(json!({}))
            )
            .kind(),
            "notion.read_page"
        );
        assert_eq!(
            NotionQueryDatabase::new(
                NotionAuth::Integration("t".into()),
                RecordingClient::ok(json!({}))
            )
            .kind(),
            "notion.query_database"
        );
    }

    #[test]
    fn connector_publishes_schemas() {
        let c = NotionConnector;
        assert_eq!(c.provider(), "notion");
        assert_eq!(c.action_schemas().len(), 3);
    }
}

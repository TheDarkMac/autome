//! Connecteur Slack (F4.1, refactoré sous le SDK en F4.5).
//!
//! Construit sur [`crate::sdk`] : auth via `Credentials`, interprétation
//! des réponses via [`SlackInterpreter`], helpers HTTP partagés.

use std::collections::BTreeMap;

use autome_core::{Action, Error, Result};
use serde_json::{Map, Value};

use crate::http::{HttpClient, HttpRequest};
use crate::sdk::{
    headers_for, interpret, ActionSchema, Connector, Credentials, ParamKind, ParamSpec,
    ResponseInterpreter,
};

const SLACK_API: &str = "https://slack.com/api";

/// Stratégie d'authentification Slack.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlackAuth {
    /// Bot/User token (`xoxb-…` ou `xoxp-…`).
    Bot(String),
}

impl Credentials for SlackAuth {
    fn apply(&self, headers: &mut BTreeMap<String, String>) {
        match self {
            SlackAuth::Bot(t) => {
                headers.insert("Authorization".into(), format!("Bearer {t}"));
            }
        }
    }
}

/// Interpréteur de réponses Slack (réponse `200` avec `{ok:false,error}` →
/// erreur ; HTTP non-2xx → erreur fallback sur le body).
pub struct SlackInterpreter;

impl ResponseInterpreter for SlackInterpreter {
    fn provider(&self) -> &'static str {
        "slack"
    }
    fn on_success(&self, body: Value) -> Result<Value> {
        let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if ok {
            return Ok(body);
        }
        let code = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        Err(Error::Invalid(format!("slack: {code}")))
    }
}

/// Connecteur Slack — registre des actions exposées.
pub struct SlackConnector;

impl Connector for SlackConnector {
    fn provider(&self) -> &'static str {
        "slack"
    }
    fn action_schemas(&self) -> Vec<ActionSchema> {
        vec![send_message_schema(), read_channel_schema()]
    }
}

/// Schéma de l'action `slack.send_message` (F4.5.c).
pub fn send_message_schema() -> ActionSchema {
    ActionSchema {
        provider: "slack",
        kind: "slack.send_message",
        label: "Slack — Envoyer un message",
        description: "Publie un message dans un canal Slack via `chat.postMessage`.",
        params: vec![
            ParamSpec::req_str("channel", "Canal"),
            ParamSpec::req_text("text", "Texte"),
            ParamSpec::opt_str("thread_ts", "Thread (parent ts)"),
            ParamSpec {
                key: "blocks",
                label: "Blocs (Block Kit)",
                kind: ParamKind::Array {
                    inner: Box::new(ParamKind::Object),
                },
                required: false,
                description: None,
            },
        ],
    }
}

/// Schéma de l'action `slack.read_channel` (F4.5.c).
pub fn read_channel_schema() -> ActionSchema {
    ActionSchema {
        provider: "slack",
        kind: "slack.read_channel",
        label: "Slack — Lire l'historique",
        description: "Récupère l'historique d'un canal via `conversations.history`.",
        params: vec![
            ParamSpec::req_str("channel", "Canal"),
            ParamSpec::opt_int("limit", "Limite"),
            ParamSpec::opt_str("cursor", "Curseur"),
            ParamSpec::opt_str("oldest", "Depuis (ts)"),
            ParamSpec::opt_str("latest", "Jusqu'à (ts)"),
        ],
    }
}

// ────────── send_message (F4.1.b) ──────────

/// Action `chat.postMessage`.
pub struct SlackSendMessage<C: HttpClient> {
    auth: SlackAuth,
    client: C,
}

impl<C: HttpClient> SlackSendMessage<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: SlackAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let channel = params
            .get("channel")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("slack send_message : `channel` requis".into()))?;
        let text = params
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("slack send_message : `text` requis".into()))?;
        let mut body = Map::new();
        body.insert("channel".into(), Value::String(channel.into()));
        body.insert("text".into(), Value::String(text.into()));
        for key in [
            "thread_ts",
            "blocks",
            "attachments",
            "username",
            "icon_emoji",
        ] {
            if let Some(v) = params.get(key) {
                body.insert(key.into(), v.clone());
            }
        }
        Ok(HttpRequest {
            method: "POST".into(),
            url: format!("{SLACK_API}/chat.postMessage"),
            headers: headers_for(&self.auth, true),
            body: Some(Value::Object(body)),
        })
    }
}

impl<C: HttpClient> Action for SlackSendMessage<C> {
    fn kind(&self) -> &'static str {
        "slack.send_message"
    }

    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("slack send_message : {e}")))?;
        interpret(resp, &SlackInterpreter)
    }
}

// ────────── read_channel (F4.1.c) ──────────

/// Action `conversations.history`.
pub struct SlackReadChannel<C: HttpClient> {
    auth: SlackAuth,
    client: C,
}

impl<C: HttpClient> SlackReadChannel<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: SlackAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let channel = params
            .get("channel")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("slack read_channel : `channel` requis".into()))?;
        let mut pairs: Vec<(String, String)> = vec![("channel".into(), channel.into())];
        for key in ["limit", "cursor", "oldest", "latest", "inclusive"] {
            if let Some(v) = params.get(key) {
                let s = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                pairs.push((key.into(), s));
            }
        }
        let query = crate::sdk::to_query(&pairs);
        Ok(HttpRequest {
            method: "GET".into(),
            url: format!("{SLACK_API}/conversations.history?{query}"),
            headers: headers_for(&self.auth, false),
            body: None,
        })
    }
}

impl<C: HttpClient> Action for SlackReadChannel<C> {
    fn kind(&self) -> &'static str {
        "slack.read_channel"
    }

    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("slack read_channel : {e}")))?;
        interpret(resp, &SlackInterpreter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::RecordingClient;
    use serde_json::json;

    // ───── auth ─────

    #[test]
    fn send_message_injects_bearer_and_json_content_type() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("xoxb-abc".into()),
            RecordingClient::ok(json!({ "ok": true, "ts": "1.0" })),
        );
        act.execute(&json!({ "channel": "C1", "text": "hi" }))
            .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(
            sent.headers.get("Authorization").map(String::as_str),
            Some("Bearer xoxb-abc")
        );
        assert_eq!(
            sent.headers.get("Content-Type").map(String::as_str),
            Some("application/json; charset=utf-8")
        );
    }

    #[test]
    fn read_channel_injects_bearer() {
        let act = SlackReadChannel::new(
            SlackAuth::Bot("xoxb-z".into()),
            RecordingClient::ok(json!({ "ok": true, "messages": [] })),
        );
        act.execute(&json!({ "channel": "C1" })).unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(
            sent.headers.get("Authorization").map(String::as_str),
            Some("Bearer xoxb-z")
        );
        // GET → pas de Content-Type avec le SDK
        assert!(!sent.headers.contains_key("Content-Type"));
    }

    // ───── send_message ─────

    #[test]
    fn send_message_builds_post_to_chat_postmessage() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({ "ok": true, "channel": "C1", "ts": "1.0" })),
        );
        let out = act
            .execute(&json!({
                "channel": "C1",
                "text": "hello",
                "thread_ts": "1700000000.000100",
                "blocks": [{ "type": "section" }],
            }))
            .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.url, "https://slack.com/api/chat.postMessage");
        let body = sent.body.expect("body present");
        assert_eq!(body["channel"], json!("C1"));
        assert_eq!(body["text"], json!("hello"));
        assert_eq!(body["thread_ts"], json!("1700000000.000100"));
        assert_eq!(body["blocks"], json!([{ "type": "section" }]));
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["channel"], json!("C1"));
    }

    #[test]
    fn send_message_ignores_unknown_optional_fields() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({ "ok": true })),
        );
        act.execute(&json!({ "channel": "C1", "text": "hi", "lol": 42 }))
            .unwrap();
        let sent = act.client.last().unwrap();
        let body = sent.body.unwrap();
        assert!(body.get("lol").is_none());
    }

    #[test]
    fn send_message_requires_channel_and_text() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({ "ok": true })),
        );
        let err = act.execute(&json!({ "text": "hi" })).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("channel")));

        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({ "ok": true })),
        );
        let err = act.execute(&json!({ "channel": "C1" })).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("text")));
    }

    // ───── read_channel ─────

    #[test]
    fn read_channel_builds_get_with_query_string() {
        let act = SlackReadChannel::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({ "ok": true, "messages": [], "has_more": false })),
        );
        act.execute(&json!({
            "channel": "C1",
            "limit": 50,
            "cursor": "dXNlcjpVMDYxTkZUVDI=",
            "inclusive": true,
        }))
        .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "GET");
        assert!(
            sent.url
                .starts_with("https://slack.com/api/conversations.history?"),
            "url={}",
            sent.url
        );
        assert!(sent.url.contains("channel=C1"));
        assert!(sent.url.contains("limit=50"));
        assert!(sent.url.contains("cursor=dXNlcjpVMDYxTkZUVDI%3D"));
        assert!(sent.url.contains("inclusive=true"));
        assert!(sent.body.is_none());
    }

    #[test]
    fn read_channel_returns_full_body_on_success() {
        let act = SlackReadChannel::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({
                "ok": true,
                "messages": [{ "ts": "1.0", "text": "hi" }],
                "has_more": false,
                "response_metadata": { "next_cursor": "" },
            })),
        );
        let out = act.execute(&json!({ "channel": "C1" })).unwrap();
        assert_eq!(out["messages"][0]["text"], json!("hi"));
        assert_eq!(out["has_more"], json!(false));
    }

    #[test]
    fn read_channel_requires_channel() {
        let act = SlackReadChannel::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({ "ok": true })),
        );
        let err = act.execute(&json!({})).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("channel")));
    }

    // ───── mapping erreurs ─────

    #[test]
    fn slack_logical_error_maps_to_invalid_with_code() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({ "ok": false, "error": "channel_not_found" })),
        );
        let err = act
            .execute(&json!({ "channel": "C1", "text": "x" }))
            .unwrap_err();
        match err {
            Error::Invalid(msg) => assert!(msg.contains("channel_not_found"), "msg={msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn slack_logical_error_without_code_uses_fallback() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::ok(json!({ "ok": false })),
        );
        let err = act
            .execute(&json!({ "channel": "C1", "text": "x" }))
            .unwrap_err();
        match err {
            Error::Invalid(msg) => assert!(msg.contains("unknown_error"), "msg={msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn http_non_2xx_maps_to_invalid() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::with_status(500, json!({ "error": "boom" })),
        );
        let err = act
            .execute(&json!({ "channel": "C1", "text": "x" }))
            .unwrap_err();
        match err {
            Error::Invalid(msg) => assert!(msg.contains("http 500"), "msg={msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn transport_error_is_propagated() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            RecordingClient::failing("connexion refusée"),
        );
        let err = act
            .execute(&json!({ "channel": "C1", "text": "x" }))
            .unwrap_err();
        match err {
            Error::Invalid(msg) => assert!(msg.contains("connexion refusée"), "msg={msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn kinds_are_stable() {
        let s = SlackSendMessage::new(SlackAuth::Bot("t".into()), RecordingClient::ok(json!({})));
        let r = SlackReadChannel::new(SlackAuth::Bot("t".into()), RecordingClient::ok(json!({})));
        assert_eq!(s.kind(), "slack.send_message");
        assert_eq!(r.kind(), "slack.read_channel");
    }

    #[test]
    fn connector_publishes_schemas() {
        let c = SlackConnector;
        assert_eq!(c.provider(), "slack");
        let schemas = c.action_schemas();
        assert_eq!(schemas.len(), 2);
        let kinds: Vec<&str> = schemas.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&"slack.send_message"));
        assert!(kinds.contains(&"slack.read_channel"));
    }
}

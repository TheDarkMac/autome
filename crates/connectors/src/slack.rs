//! Connecteur Slack (F4.1) — actions `send_message` et `read_channel`.
//!
//! Mêmes principes que [`crate::HttpAction`] : aucune dépendance réseau —
//! le transport est injecté via [`crate::HttpClient`]. L'authentification
//! se limite ici au **bot/user token** (`xoxb-…` / `xoxp-…`) injecté en
//! `Authorization: Bearer …` (F4.1.a) ; le flow OAuth complet relève de
//! F6.2.
//!
//! Schéma des `params` :
//! ```json
//! // send_message
//! { "channel": "C123", "text": "hello",
//!   "thread_ts": "1700000000.000100", "blocks": [ … ] }
//! // read_channel
//! { "channel": "C123", "limit": 50, "cursor": "…",
//!   "oldest": "…", "latest": "…", "inclusive": true }
//! ```
//!
//! Le mapping d'erreur (F4.1.d) traduit :
//! - réponse Slack `{ok:false, error}` → [`autome_core::Error::Invalid`]
//!   `"slack: <code>"` ;
//! - statut HTTP non-2xx → `"slack: http <status> — …"` ;
//! - échec transport → `"slack <action> : <message>"`.
//!
//! Le moteur (F2.7) décide ensuite retry/fallback à partir du message.

use std::collections::BTreeMap;

use autome_core::{Action, Error, Result};
use serde_json::{Map, Value};

use crate::http::{HttpClient, HttpRequest, HttpResponse};

/// Racine de l'API Web Slack.
const SLACK_API: &str = "https://slack.com/api";

/// Stratégie d'authentification Slack.
///
/// La variante OAuth (refresh tokens, multi-provider) relève de F6.2 ;
/// pour F4.1, seul le bot/user token statique est nécessaire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackAuth {
    /// Bot/User token (`xoxb-…` ou `xoxp-…`).
    Bot(String),
}

impl SlackAuth {
    fn bearer(&self) -> String {
        match self {
            SlackAuth::Bot(t) => format!("Bearer {t}"),
        }
    }
}

/// En-têtes communs à toutes les requêtes Slack (auth + JSON).
fn common_headers(auth: &SlackAuth) -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    h.insert("Authorization".into(), auth.bearer());
    h.insert(
        "Content-Type".into(),
        "application/json; charset=utf-8".into(),
    );
    h
}

/// Traduit la réponse Slack en `Result<Value>` (F4.1.d).
///
/// Slack répond presque toujours `200 OK` avec un champ `ok` ; un statut
/// non-2xx est traité comme erreur définitive (4xx) ou retryable (5xx)
/// par le moteur via le message.
fn interpret(resp: HttpResponse) -> Result<Value> {
    if !(200..300).contains(&resp.status) {
        return Err(Error::Invalid(format!(
            "slack: http {} — {}",
            resp.status, resp.body
        )));
    }
    let ok = resp
        .body
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if ok {
        Ok(resp.body)
    } else {
        let code = resp
            .body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        Err(Error::Invalid(format!("slack: {code}")))
    }
}

/// Encodage URL minimal (RFC 3986 « unreserved »).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Action `chat.postMessage` (F4.1.b).
///
/// Params requis : `channel`, `text`. Champs additionnels propagés tels
/// quels au body JSON : `thread_ts`, `blocks`, `attachments`, `username`,
/// `icon_emoji`.
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
            headers: common_headers(&self.auth),
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
        interpret(resp)
    }
}

/// Action `conversations.history` (F4.1.c).
///
/// Params requis : `channel`. Champs propagés en query string :
/// `limit`, `cursor`, `oldest`, `latest`, `inclusive`.
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
        // Liste fixe pour un ordre déterministe dans l'URL (utile en tests).
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
        let query = pairs
            .into_iter()
            .map(|(k, v)| format!("{}={}", percent_encode(&k), percent_encode(&v)))
            .collect::<Vec<_>>()
            .join("&");
        Ok(HttpRequest {
            method: "GET".into(),
            url: format!("{SLACK_API}/conversations.history?{query}"),
            headers: common_headers(&self.auth),
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
        interpret(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    /// Transport factice : enregistre la dernière requête, renvoie une
    /// réponse canned (ou une erreur de transport si `fail`).
    struct MockClient {
        last: RefCell<Option<HttpRequest>>,
        resp: HttpResponse,
        fail: Option<String>,
    }

    impl MockClient {
        fn ok(body: Value) -> Self {
            Self {
                last: RefCell::new(None),
                resp: HttpResponse { status: 200, body },
                fail: None,
            }
        }
        fn with_status(status: u16, body: Value) -> Self {
            Self {
                last: RefCell::new(None),
                resp: HttpResponse { status, body },
                fail: None,
            }
        }
        fn failing(msg: &str) -> Self {
            Self {
                last: RefCell::new(None),
                resp: HttpResponse {
                    status: 200,
                    body: json!({}),
                },
                fail: Some(msg.into()),
            }
        }
    }

    impl HttpClient for MockClient {
        fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
            *self.last.borrow_mut() = Some(req.clone());
            if let Some(m) = &self.fail {
                Err(m.clone())
            } else {
                Ok(self.resp.clone())
            }
        }
    }

    // F4.1.a — auth : injection systématique des en-têtes
    #[test]
    fn send_message_injects_bearer_and_json_content_type() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("xoxb-abc".into()),
            MockClient::ok(json!({ "ok": true, "ts": "1.0" })),
        );
        act.execute(&json!({ "channel": "C1", "text": "hi" }))
            .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
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
            MockClient::ok(json!({ "ok": true, "messages": [] })),
        );
        act.execute(&json!({ "channel": "C1" })).unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(
            sent.headers.get("Authorization").map(String::as_str),
            Some("Bearer xoxb-z")
        );
    }

    // F4.1.b — chat.postMessage : POST + body JSON + champs optionnels
    #[test]
    fn send_message_builds_post_to_chat_postmessage() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            MockClient::ok(json!({ "ok": true, "channel": "C1", "ts": "1.0" })),
        );
        let out = act
            .execute(&json!({
                "channel": "C1",
                "text": "hello",
                "thread_ts": "1700000000.000100",
                "blocks": [{ "type": "section" }],
            }))
            .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
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
            MockClient::ok(json!({ "ok": true })),
        );
        act.execute(&json!({ "channel": "C1", "text": "hi", "lol": 42 }))
            .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        let body = sent.body.unwrap();
        assert!(body.get("lol").is_none());
    }

    #[test]
    fn send_message_requires_channel_and_text() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            MockClient::ok(json!({ "ok": true })),
        );
        let err = act.execute(&json!({ "text": "hi" })).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("channel")));

        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            MockClient::ok(json!({ "ok": true })),
        );
        let err = act.execute(&json!({ "channel": "C1" })).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("text")));
    }

    // F4.1.c — conversations.history : GET + query encodée
    #[test]
    fn read_channel_builds_get_with_query_string() {
        let act = SlackReadChannel::new(
            SlackAuth::Bot("t".into()),
            MockClient::ok(json!({ "ok": true, "messages": [], "has_more": false })),
        );
        act.execute(&json!({
            "channel": "C1",
            "limit": 50,
            "cursor": "dXNlcjpVMDYxTkZUVDI=",
            "inclusive": true,
        }))
        .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "GET");
        assert!(
            sent.url
                .starts_with("https://slack.com/api/conversations.history?"),
            "url={}",
            sent.url
        );
        assert!(sent.url.contains("channel=C1"));
        assert!(sent.url.contains("limit=50"));
        // `=` (0x3D) doit être pourcent-encodé dans la valeur.
        assert!(sent.url.contains("cursor=dXNlcjpVMDYxTkZUVDI%3D"));
        assert!(sent.url.contains("inclusive=true"));
        assert!(sent.body.is_none());
    }

    #[test]
    fn read_channel_returns_full_body_on_success() {
        let act = SlackReadChannel::new(
            SlackAuth::Bot("t".into()),
            MockClient::ok(json!({
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
            MockClient::ok(json!({ "ok": true })),
        );
        let err = act.execute(&json!({})).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("channel")));
    }

    // F4.1.d — mapping erreurs
    #[test]
    fn slack_logical_error_maps_to_invalid_with_code() {
        let act = SlackSendMessage::new(
            SlackAuth::Bot("t".into()),
            MockClient::ok(json!({ "ok": false, "error": "channel_not_found" })),
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
            MockClient::ok(json!({ "ok": false })),
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
            MockClient::with_status(500, json!({ "error": "boom" })),
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
            MockClient::failing("connection refused"),
        );
        let err = act
            .execute(&json!({ "channel": "C1", "text": "x" }))
            .unwrap_err();
        match err {
            Error::Invalid(msg) => assert!(msg.contains("connection refused"), "msg={msg}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn kinds_are_stable() {
        let s = SlackSendMessage::new(SlackAuth::Bot("t".into()), MockClient::ok(json!({})));
        let r = SlackReadChannel::new(SlackAuth::Bot("t".into()), MockClient::ok(json!({})));
        assert_eq!(s.kind(), "slack.send_message");
        assert_eq!(r.kind(), "slack.read_channel");
    }
}

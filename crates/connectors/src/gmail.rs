//! Connecteur Gmail (F4.2, refactoré sous le SDK en F4.5).
//!
//! Construit sur [`crate::sdk`] : auth via `Credentials`, interprétation
//! des réponses via [`GmailInterpreter`], helpers HTTP partagés.
//!
//! Le message est construit en RFC 822 puis encodé en base64url **sans
//! padding** (Gmail API impose ce format pour le champ `raw`).

use std::collections::BTreeMap;

use autome_core::{Action, Error, Result};
use serde_json::{json, Value};

use crate::http::{HttpClient, HttpRequest};
use crate::sdk::{
    headers_for, interpret, percent_encode, to_query, ActionSchema, Connector, Credentials,
    ParamKind, ParamSpec, ResponseInterpreter,
};

const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1";

/// Stratégie d'authentification Gmail.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GmailAuth {
    /// Access token OAuth2 (déjà rafraîchi en amont — voir F6.2).
    OAuth2(String),
}

impl Credentials for GmailAuth {
    fn apply(&self, headers: &mut BTreeMap<String, String>) {
        match self {
            GmailAuth::OAuth2(t) => {
                headers.insert("Authorization".into(), format!("Bearer {t}"));
            }
        }
    }
}

/// Interpréteur de réponses Gmail (extrait `error.message` Google sur les non-2xx).
pub struct GmailInterpreter;

impl ResponseInterpreter for GmailInterpreter {
    fn provider(&self) -> &'static str {
        "gmail"
    }
    fn extract_error(&self, body: &Value) -> Option<String> {
        body.get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// Connecteur Gmail — registre des actions.
pub struct GmailConnector;

impl Connector for GmailConnector {
    fn provider(&self) -> &'static str {
        "gmail"
    }
    fn action_schemas(&self) -> Vec<ActionSchema> {
        vec![
            send_email_schema(),
            list_messages_schema(),
            read_message_schema(),
            get_attachment_schema(),
        ]
    }
}

// ──────────────────── helpers MIME ────────────────────

/// Alphabet base64url (RFC 4648 §5).
const B64URL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encodage base64url **sans padding** (format imposé par Gmail pour `raw`).
fn base64_url_no_pad(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for c in chunks.by_ref() {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out.push(B64URL[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64URL[((n >> 12) & 0x3F) as usize] as char);
        out.push(B64URL[((n >> 6) & 0x3F) as usize] as char);
        out.push(B64URL[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(B64URL[((n >> 18) & 0x3F) as usize] as char);
            out.push(B64URL[((n >> 12) & 0x3F) as usize] as char);
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(B64URL[((n >> 18) & 0x3F) as usize] as char);
            out.push(B64URL[((n >> 12) & 0x3F) as usize] as char);
            out.push(B64URL[((n >> 6) & 0x3F) as usize] as char);
        }
        _ => {}
    }
    out
}

fn collect_addresses(v: &Value) -> Vec<String> {
    if let Some(s) = v.as_str() {
        return vec![s.to_string()];
    }
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

struct ParsedAttachment {
    filename: String,
    content_type: String,
    content_base64: String,
}

fn collect_attachments(params: &Value) -> Result<Vec<ParsedAttachment>> {
    let Some(arr) = params.get("attachments").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    arr.iter()
        .map(|a| {
            let filename = a
                .get("filename")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid("gmail send_email : `filename` requis".into()))?
                .to_string();
            let content_type = a
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string();
            let content_base64 = a
                .get("content_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid("gmail send_email : `content_base64` requis".into()))?
                .to_string();
            Ok(ParsedAttachment {
                filename,
                content_type,
                content_base64,
            })
        })
        .collect()
}

/// Construit le message RFC 822 (texte + pièces jointes éventuelles).
fn render_rfc822(
    from: Option<&str>,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body: &str,
    attachments: &[ParsedAttachment],
) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(from) = from {
        lines.push(format!("From: {from}"));
    }
    if !to.is_empty() {
        lines.push(format!("To: {}", to.join(", ")));
    }
    if !cc.is_empty() {
        lines.push(format!("Cc: {}", cc.join(", ")));
    }
    if !bcc.is_empty() {
        lines.push(format!("Bcc: {}", bcc.join(", ")));
    }
    lines.push(format!("Subject: {subject}"));
    lines.push("MIME-Version: 1.0".into());

    if attachments.is_empty() {
        lines.push("Content-Type: text/plain; charset=utf-8".into());
        lines.push("Content-Transfer-Encoding: 8bit".into());
        lines.push(String::new());
        lines.push(body.into());
        return lines.join("\r\n");
    }

    let boundary = "----=_AutomeBoundary_8a5f1c3d";
    lines.push(format!(
        "Content-Type: multipart/mixed; boundary=\"{boundary}\""
    ));
    lines.push(String::new());
    lines.push(format!("--{boundary}"));
    lines.push("Content-Type: text/plain; charset=utf-8".into());
    lines.push("Content-Transfer-Encoding: 8bit".into());
    lines.push(String::new());
    lines.push(body.into());
    for att in attachments {
        lines.push(format!("--{boundary}"));
        lines.push(format!(
            "Content-Type: {}; name=\"{}\"",
            att.content_type, att.filename
        ));
        lines.push("Content-Transfer-Encoding: base64".into());
        lines.push(format!(
            "Content-Disposition: attachment; filename=\"{}\"",
            att.filename
        ));
        lines.push(String::new());
        lines.push(att.content_base64.clone());
    }
    lines.push(format!("--{boundary}--"));
    lines.join("\r\n")
}

fn user_id_or_default(params: &Value) -> String {
    params
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or("me")
        .to_string()
}

// ───────────────────────── Actions ─────────────────────────

/// Action `users.messages.send` (F4.2.b + F4.2.d).
pub struct GmailSendEmail<C: HttpClient> {
    auth: GmailAuth,
    client: C,
}

impl<C: HttpClient> GmailSendEmail<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GmailAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let to_raw = params
            .get("to")
            .ok_or_else(|| Error::Invalid("gmail send_email : `to` requis".into()))?;
        let to = collect_addresses(to_raw);
        if to.is_empty() {
            return Err(Error::Invalid(
                "gmail send_email : `to` doit contenir au moins une adresse".into(),
            ));
        }
        let subject = params
            .get("subject")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("gmail send_email : `subject` requis".into()))?;
        let body = params
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("gmail send_email : `body` requis".into()))?;
        let from = params.get("from").and_then(Value::as_str);
        let cc = params.get("cc").map(collect_addresses).unwrap_or_default();
        let bcc = params.get("bcc").map(collect_addresses).unwrap_or_default();
        let attachments = collect_attachments(params)?;
        let rfc822 = render_rfc822(from, &to, &cc, &bcc, subject, body, &attachments);
        let raw = base64_url_no_pad(rfc822.as_bytes());
        let user_id = user_id_or_default(params);
        Ok(HttpRequest {
            method: "POST".into(),
            url: format!(
                "{GMAIL_API}/users/{}/messages/send",
                percent_encode(&user_id)
            ),
            headers: headers_for(&self.auth, true),
            body: Some(json!({ "raw": raw })),
        })
    }
}

impl<C: HttpClient> Action for GmailSendEmail<C> {
    fn kind(&self) -> &'static str {
        "gmail.send_email"
    }

    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("gmail send_email : {e}")))?;
        interpret(resp, &GmailInterpreter)
    }
}

/// Action `users.messages.list` (F4.2.c).
pub struct GmailListMessages<C: HttpClient> {
    auth: GmailAuth,
    client: C,
}

impl<C: HttpClient> GmailListMessages<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GmailAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let user_id = user_id_or_default(params);
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Some(q) = params.get("q").and_then(Value::as_str) {
            pairs.push(("q".into(), q.into()));
        }
        if let Some(n) = params.get("max_results") {
            let s = n
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| n.to_string());
            pairs.push(("maxResults".into(), s));
        }
        if let Some(t) = params.get("page_token").and_then(Value::as_str) {
            pairs.push(("pageToken".into(), t.into()));
        }
        if let Some(arr) = params.get("label_ids").and_then(Value::as_array) {
            for v in arr {
                if let Some(s) = v.as_str() {
                    pairs.push(("labelIds".into(), s.into()));
                }
            }
        }
        let mut url = format!("{GMAIL_API}/users/{}/messages", percent_encode(&user_id));
        if !pairs.is_empty() {
            url.push('?');
            url.push_str(&to_query(&pairs));
        }
        Ok(HttpRequest {
            method: "GET".into(),
            url,
            headers: headers_for(&self.auth, false),
            body: None,
        })
    }
}

impl<C: HttpClient> Action for GmailListMessages<C> {
    fn kind(&self) -> &'static str {
        "gmail.list_messages"
    }

    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("gmail list_messages : {e}")))?;
        interpret(resp, &GmailInterpreter)
    }
}

/// Action `users.messages.get` (F4.2.c).
pub struct GmailReadMessage<C: HttpClient> {
    auth: GmailAuth,
    client: C,
}

impl<C: HttpClient> GmailReadMessage<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GmailAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("gmail read_message : `id` requis".into()))?;
        let user_id = user_id_or_default(params);
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Some(fmt) = params.get("format").and_then(Value::as_str) {
            pairs.push(("format".into(), fmt.into()));
        }
        let mut url = format!(
            "{GMAIL_API}/users/{}/messages/{}",
            percent_encode(&user_id),
            percent_encode(id)
        );
        if !pairs.is_empty() {
            url.push('?');
            url.push_str(&to_query(&pairs));
        }
        Ok(HttpRequest {
            method: "GET".into(),
            url,
            headers: headers_for(&self.auth, false),
            body: None,
        })
    }
}

impl<C: HttpClient> Action for GmailReadMessage<C> {
    fn kind(&self) -> &'static str {
        "gmail.read_message"
    }

    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("gmail read_message : {e}")))?;
        interpret(resp, &GmailInterpreter)
    }
}

/// Action `users.messages.attachments.get` (F4.2.d).
pub struct GmailGetAttachment<C: HttpClient> {
    auth: GmailAuth,
    client: C,
}

impl<C: HttpClient> GmailGetAttachment<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GmailAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let message_id = params
            .get("message_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("gmail get_attachment : `message_id` requis".into()))?;
        let attachment_id = params
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::Invalid("gmail get_attachment : `attachment_id` requis".into())
            })?;
        let user_id = user_id_or_default(params);
        let url = format!(
            "{GMAIL_API}/users/{}/messages/{}/attachments/{}",
            percent_encode(&user_id),
            percent_encode(message_id),
            percent_encode(attachment_id)
        );
        Ok(HttpRequest {
            method: "GET".into(),
            url,
            headers: headers_for(&self.auth, false),
            body: None,
        })
    }
}

impl<C: HttpClient> Action for GmailGetAttachment<C> {
    fn kind(&self) -> &'static str {
        "gmail.get_attachment"
    }

    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("gmail get_attachment : {e}")))?;
        interpret(resp, &GmailInterpreter)
    }
}

// ───────────────────────── Schémas (F4.5.c) ─────────────────────────

/// Schéma de l'action `gmail.send_email`.
pub fn send_email_schema() -> ActionSchema {
    ActionSchema {
        provider: "gmail",
        kind: "gmail.send_email",
        label: "Gmail — Envoyer un mail",
        description: "Envoie un message via `users.messages.send`.",
        params: vec![
            ParamSpec {
                key: "to",
                label: "Destinataire(s)",
                kind: ParamKind::Array {
                    inner: Box::new(ParamKind::String),
                },
                required: true,
                description: None,
            },
            ParamSpec {
                key: "cc",
                label: "Cc",
                kind: ParamKind::Array {
                    inner: Box::new(ParamKind::String),
                },
                required: false,
                description: None,
            },
            ParamSpec {
                key: "bcc",
                label: "Cci",
                kind: ParamKind::Array {
                    inner: Box::new(ParamKind::String),
                },
                required: false,
                description: None,
            },
            ParamSpec::opt_str("from", "From"),
            ParamSpec::req_str("subject", "Sujet"),
            ParamSpec::req_text("body", "Corps"),
            ParamSpec {
                key: "attachments",
                label: "Pièces jointes",
                kind: ParamKind::Array {
                    inner: Box::new(ParamKind::Object),
                },
                required: false,
                description: None,
            },
            ParamSpec::opt_str("user_id", "User id (défaut: me)"),
        ],
    }
}

/// Schéma de l'action `gmail.list_messages`.
pub fn list_messages_schema() -> ActionSchema {
    ActionSchema {
        provider: "gmail",
        kind: "gmail.list_messages",
        label: "Gmail — Lister les messages",
        description: "Liste les messages via `users.messages.list`.",
        params: vec![
            ParamSpec::opt_str("user_id", "User id (défaut: me)"),
            ParamSpec::opt_str("q", "Recherche (`q`)"),
            ParamSpec::opt_int("max_results", "Limite"),
            ParamSpec::opt_str("page_token", "Page token"),
            ParamSpec {
                key: "label_ids",
                label: "Labels",
                kind: ParamKind::Array {
                    inner: Box::new(ParamKind::String),
                },
                required: false,
                description: None,
            },
        ],
    }
}

/// Schéma de l'action `gmail.read_message`.
pub fn read_message_schema() -> ActionSchema {
    ActionSchema {
        provider: "gmail",
        kind: "gmail.read_message",
        label: "Gmail — Lire un message",
        description: "Lit un message via `users.messages.get`.",
        params: vec![
            ParamSpec::req_str("id", "ID du message"),
            ParamSpec::opt_str("user_id", "User id (défaut: me)"),
            ParamSpec {
                key: "format",
                label: "Format",
                kind: ParamKind::Enum {
                    variants: vec!["full", "metadata", "minimal", "raw"],
                },
                required: false,
                description: None,
            },
        ],
    }
}

/// Schéma de l'action `gmail.get_attachment`.
pub fn get_attachment_schema() -> ActionSchema {
    ActionSchema {
        provider: "gmail",
        kind: "gmail.get_attachment",
        label: "Gmail — Récupérer une pièce jointe",
        description: "Récupère une PJ via `users.messages.attachments.get`.",
        params: vec![
            ParamSpec::req_str("message_id", "Message ID"),
            ParamSpec::req_str("attachment_id", "Attachment ID"),
            ParamSpec::opt_str("user_id", "User id (défaut: me)"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::RecordingClient;
    use serde_json::json;

    fn base64_url_decode(s: &str) -> Vec<u8> {
        let value = |c: u8| -> u32 {
            match c {
                b'A'..=b'Z' => u32::from(c - b'A'),
                b'a'..=b'z' => u32::from(c - b'a') + 26,
                b'0'..=b'9' => u32::from(c - b'0') + 52,
                b'-' => 62,
                b'_' => 63,
                _ => panic!("char base64url invalide: {c:?}"),
            }
        };
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
        let mut i = 0;
        while i + 4 <= bytes.len() {
            let n = (value(bytes[i]) << 18)
                | (value(bytes[i + 1]) << 12)
                | (value(bytes[i + 2]) << 6)
                | value(bytes[i + 3]);
            out.push(((n >> 16) & 0xFF) as u8);
            out.push(((n >> 8) & 0xFF) as u8);
            out.push((n & 0xFF) as u8);
            i += 4;
        }
        match bytes.len() - i {
            2 => {
                let n = (value(bytes[i]) << 18) | (value(bytes[i + 1]) << 12);
                out.push(((n >> 16) & 0xFF) as u8);
            }
            3 => {
                let n = (value(bytes[i]) << 18)
                    | (value(bytes[i + 1]) << 12)
                    | (value(bytes[i + 2]) << 6);
                out.push(((n >> 16) & 0xFF) as u8);
                out.push(((n >> 8) & 0xFF) as u8);
            }
            _ => {}
        }
        out
    }

    #[test]
    fn base64_url_encodes_known_vectors() {
        assert_eq!(base64_url_no_pad(b""), "");
        assert_eq!(base64_url_no_pad(b"f"), "Zg");
        assert_eq!(base64_url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64_url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64_url_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64_url_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64_url_no_pad(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_url_no_pad(&[0xFB, 0xEF, 0xFF]), "--__");
    }

    #[test]
    fn all_actions_inject_bearer() {
        let auth = GmailAuth::OAuth2("tok-1".into());

        let send = GmailSendEmail::new(auth.clone(), RecordingClient::ok(json!({ "id": "x" })));
        send.execute(&json!({ "to": "b@x", "subject": "s", "body": "b" }))
            .unwrap();
        assert_eq!(
            send.client
                .last()
                .unwrap()
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer tok-1")
        );

        let list =
            GmailListMessages::new(auth.clone(), RecordingClient::ok(json!({ "messages": [] })));
        list.execute(&json!({})).unwrap();
        assert_eq!(
            list.client
                .last()
                .unwrap()
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer tok-1")
        );

        let read = GmailReadMessage::new(auth.clone(), RecordingClient::ok(json!({ "id": "a" })));
        read.execute(&json!({ "id": "a" })).unwrap();
        assert_eq!(
            read.client
                .last()
                .unwrap()
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer tok-1")
        );

        let att = GmailGetAttachment::new(auth, RecordingClient::ok(json!({ "data": "" })));
        att.execute(&json!({ "message_id": "m", "attachment_id": "a" }))
            .unwrap();
        assert_eq!(
            att.client
                .last()
                .unwrap()
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer tok-1")
        );
    }

    #[test]
    fn send_email_builds_post_with_base64url_raw() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "msg1", "threadId": "t1" })),
        );
        let out = act
            .execute(&json!({
                "to": "bob@example.com",
                "from": "alice@example.com",
                "subject": "Hi",
                "body": "Hello!"
            }))
            .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(
            sent.url,
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/send"
        );
        assert_eq!(
            sent.headers.get("Content-Type").map(String::as_str),
            Some("application/json; charset=utf-8")
        );
        let body = sent.body.unwrap();
        let raw = body["raw"].as_str().expect("raw must be a string");
        assert!(!raw.contains('='), "raw doit être sans padding : {raw}");
        assert!(!raw.contains('+') && !raw.contains('/'));
        let decoded = String::from_utf8(base64_url_decode(raw)).unwrap();
        assert!(decoded.contains("From: alice@example.com"));
        assert!(decoded.contains("To: bob@example.com"));
        assert!(decoded.contains("Subject: Hi"));
        assert!(decoded.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(decoded.contains("\r\n\r\nHello!"));
        assert_eq!(out["id"], json!("msg1"));
    }

    #[test]
    fn send_email_accepts_to_as_array_and_renders_cc_bcc() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        act.execute(&json!({
            "to": ["a@x", "b@x"],
            "cc": "c@x",
            "bcc": ["d@x"],
            "subject": "S",
            "body": "B"
        }))
        .unwrap();
        let raw = act.client.last().unwrap().body.unwrap()["raw"]
            .as_str()
            .unwrap()
            .to_string();
        let decoded = String::from_utf8(base64_url_decode(&raw)).unwrap();
        assert!(decoded.contains("To: a@x, b@x"));
        assert!(decoded.contains("Cc: c@x"));
        assert!(decoded.contains("Bcc: d@x"));
    }

    #[test]
    fn send_email_uses_explicit_user_id() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        act.execute(&json!({
            "user_id": "alice@example.com",
            "to": "b@x", "subject": "s", "body": "b"
        }))
        .unwrap();
        let url = act.client.last().unwrap().url;
        assert!(
            url.contains("/users/alice%40example.com/messages/send"),
            "url={url}"
        );
    }

    #[test]
    fn send_email_requires_to_subject_body() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        assert!(matches!(
            act.execute(&json!({ "subject": "s", "body": "b" })).unwrap_err(),
            Error::Invalid(ref m) if m.contains("`to`")
        ));
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        assert!(matches!(
            act.execute(&json!({ "to": "b@x", "body": "b" })).unwrap_err(),
            Error::Invalid(ref m) if m.contains("`subject`")
        ));
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        assert!(matches!(
            act.execute(&json!({ "to": "b@x", "subject": "s" })).unwrap_err(),
            Error::Invalid(ref m) if m.contains("`body`")
        ));
    }

    #[test]
    fn send_email_empty_to_array_is_an_error() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        let err = act
            .execute(&json!({ "to": [], "subject": "s", "body": "b" }))
            .unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("au moins une adresse"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn list_messages_builds_get_with_query_and_repeated_label_ids() {
        let act = GmailListMessages::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "messages": [{ "id": "a" }] })),
        );
        act.execute(&json!({
            "q": "from:foo@bar",
            "max_results": 25,
            "page_token": "next-1",
            "label_ids": ["INBOX", "STARRED"]
        }))
        .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "GET");
        assert!(
            sent.url
                .starts_with("https://gmail.googleapis.com/gmail/v1/users/me/messages?"),
            "url={}",
            sent.url
        );
        assert!(sent.url.contains("q=from%3Afoo%40bar"), "url={}", sent.url);
        assert!(sent.url.contains("maxResults=25"));
        assert!(sent.url.contains("pageToken=next-1"));
        assert!(sent.url.contains("labelIds=INBOX"));
        assert!(sent.url.contains("labelIds=STARRED"));
        assert!(sent.body.is_none());
        assert!(!sent.headers.contains_key("Content-Type"));
    }

    #[test]
    fn list_messages_works_without_query() {
        let act = GmailListMessages::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "messages": [] })),
        );
        act.execute(&json!({})).unwrap();
        let url = act.client.last().unwrap().url;
        assert_eq!(
            url,
            "https://gmail.googleapis.com/gmail/v1/users/me/messages"
        );
    }

    #[test]
    fn read_message_builds_get_with_id_and_format() {
        let act = GmailReadMessage::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "abc", "payload": {} })),
        );
        let out = act
            .execute(&json!({ "id": "abc", "format": "full" }))
            .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "GET");
        assert_eq!(
            sent.url,
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/abc?format=full"
        );
        assert!(sent.body.is_none());
        assert_eq!(out["id"], json!("abc"));
    }

    #[test]
    fn read_message_requires_id() {
        let act = GmailReadMessage::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "a" })),
        );
        let err = act.execute(&json!({})).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("`id`")));
    }

    #[test]
    fn send_email_with_attachment_builds_multipart_mixed() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        act.execute(&json!({
            "to": "b@x",
            "subject": "S",
            "body": "B",
            "attachments": [{
                "filename": "doc.pdf",
                "content_type": "application/pdf",
                "content_base64": "JVBERi0xLjQK"
            }]
        }))
        .unwrap();
        let raw = act.client.last().unwrap().body.unwrap()["raw"]
            .as_str()
            .unwrap()
            .to_string();
        let decoded = String::from_utf8(base64_url_decode(&raw)).unwrap();
        assert!(decoded.contains("Content-Type: multipart/mixed; boundary="));
        assert!(decoded.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(decoded.contains("Content-Type: application/pdf; name=\"doc.pdf\""));
        assert!(decoded.contains("Content-Transfer-Encoding: base64"));
        assert!(decoded.contains("Content-Disposition: attachment; filename=\"doc.pdf\""));
        assert!(decoded.contains("JVBERi0xLjQK"));
        assert!(decoded.contains("--\r\n") || decoded.ends_with("--"));
    }

    #[test]
    fn send_email_attachment_defaults_to_octet_stream() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        act.execute(&json!({
            "to": "b@x", "subject": "S", "body": "B",
            "attachments": [{ "filename": "f.bin", "content_base64": "AAAA" }]
        }))
        .unwrap();
        let raw = act.client.last().unwrap().body.unwrap()["raw"]
            .as_str()
            .unwrap()
            .to_string();
        let decoded = String::from_utf8(base64_url_decode(&raw)).unwrap();
        assert!(decoded.contains("Content-Type: application/octet-stream; name=\"f.bin\""));
    }

    #[test]
    fn send_email_attachment_missing_fields_errors() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        let err = act
            .execute(&json!({
                "to": "b@x", "subject": "S", "body": "B",
                "attachments": [{ "filename": "f.bin" }]
            }))
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("content_base64")));

        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "id": "x" })),
        );
        let err = act
            .execute(&json!({
                "to": "b@x", "subject": "S", "body": "B",
                "attachments": [{ "content_base64": "AAAA" }]
            }))
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("filename")));
    }

    #[test]
    fn get_attachment_builds_correct_url() {
        let act = GmailGetAttachment::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "data": "ABCD", "size": 4 })),
        );
        let out = act
            .execute(&json!({
                "message_id": "m1",
                "attachment_id": "att-1"
            }))
            .unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "GET");
        assert_eq!(
            sent.url,
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/m1/attachments/att-1"
        );
        assert!(sent.body.is_none());
        assert_eq!(out["data"], json!("ABCD"));
    }

    #[test]
    fn get_attachment_requires_message_and_attachment_ids() {
        let act = GmailGetAttachment::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "data": "" })),
        );
        let err = act.execute(&json!({ "attachment_id": "a" })).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("message_id")));

        let act = GmailGetAttachment::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::ok(json!({ "data": "" })),
        );
        let err = act.execute(&json!({ "message_id": "m" })).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("attachment_id")));
    }

    #[test]
    fn http_non_2xx_extracts_google_error_message() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::with_status(
                400,
                json!({ "error": {
                    "code": 400, "message": "Invalid To header", "status": "INVALID_ARGUMENT"
                }}),
            ),
        );
        let err = act
            .execute(&json!({ "to": "b@x", "subject": "s", "body": "b" }))
            .unwrap_err();
        match err {
            Error::Invalid(m) => {
                assert!(m.contains("http 400"), "msg={m}");
                assert!(m.contains("Invalid To header"), "msg={m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn http_non_2xx_falls_back_to_raw_body() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::with_status(500, json!({ "details": "unknown" })),
        );
        let err = act
            .execute(&json!({ "to": "b@x", "subject": "s", "body": "b" }))
            .unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("http 500"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn transport_error_is_propagated() {
        let act = GmailSendEmail::new(
            GmailAuth::OAuth2("t".into()),
            RecordingClient::failing("dns failure"),
        );
        let err = act
            .execute(&json!({ "to": "b@x", "subject": "s", "body": "b" }))
            .unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("dns failure"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn kinds_are_stable() {
        assert_eq!(
            GmailSendEmail::new(
                GmailAuth::OAuth2("t".into()),
                RecordingClient::ok(json!({}))
            )
            .kind(),
            "gmail.send_email"
        );
        assert_eq!(
            GmailListMessages::new(
                GmailAuth::OAuth2("t".into()),
                RecordingClient::ok(json!({}))
            )
            .kind(),
            "gmail.list_messages"
        );
        assert_eq!(
            GmailReadMessage::new(
                GmailAuth::OAuth2("t".into()),
                RecordingClient::ok(json!({}))
            )
            .kind(),
            "gmail.read_message"
        );
        assert_eq!(
            GmailGetAttachment::new(
                GmailAuth::OAuth2("t".into()),
                RecordingClient::ok(json!({}))
            )
            .kind(),
            "gmail.get_attachment"
        );
    }

    #[test]
    fn connector_publishes_schemas() {
        let c = GmailConnector;
        assert_eq!(c.provider(), "gmail");
        assert_eq!(c.action_schemas().len(), 4);
    }
}

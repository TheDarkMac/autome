//! Connecteur exemple `echo` (F4.5.d) — démonstration du SDK.
//!
//! Ce module n'a **aucune utilité fonctionnelle** : c'est le squelette le
//! plus court possible (~50 lignes hors tests) qu'un développeur peut
//! copier comme point de départ pour un nouveau connecteur.
//!
//! Patron suivi (voir [`crate::sdk`]) :
//! 1. Un enum `*Auth` qui implémente [`Credentials`] — il pose ses
//!    en-têtes (`Authorization` + extras propres au provider) ;
//! 2. Un struct `*Interpreter` qui implémente [`ResponseInterpreter`] —
//!    il extrait l'erreur métier d'une réponse non-2xx ;
//! 3. Une (ou plusieurs) `Action<C: HttpClient>` qui construit une
//!    [`HttpRequest`] via [`headers_for`] et délègue à `interpret` ;
//! 4. Un `*Connector` registre qui publie les [`ActionSchema`].
//!
//! Pour un connecteur réel, voir [`crate::slack`] (le plus compact),
//! [`crate::gmail`] (avec MIME et pagination), [`crate::notion`] (avec
//! helpers blocs) ou [`crate::github`] (avec crypto webhooks).

use std::collections::BTreeMap;

use autome_core::{Action, Error, Result};
use serde_json::{json, Value};

use crate::http::{HttpClient, HttpRequest};
use crate::sdk::{
    headers_for, interpret, ActionSchema, Connector, Credentials, ParamSpec, ResponseInterpreter,
};

const ECHO_API: &str = "https://echo.example.invalid/v1";

/// Auth « API key » — démo. Le vrai connecteur fera mieux que stocker
/// la clé en clair dans un enum…
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EchoAuth {
    /// Clé d'API statique.
    ApiKey(String),
}

impl Credentials for EchoAuth {
    fn apply(&self, headers: &mut BTreeMap<String, String>) {
        match self {
            EchoAuth::ApiKey(k) => {
                headers.insert("X-Api-Key".into(), k.clone());
            }
        }
    }
}

/// Interpréteur : extrait `error` du body, sinon fallback sur le body brut.
pub struct EchoInterpreter;

impl ResponseInterpreter for EchoInterpreter {
    fn provider(&self) -> &'static str {
        "echo"
    }
    fn extract_error(&self, body: &Value) -> Option<String> {
        body.get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// Connecteur de démonstration.
pub struct EchoConnector;

impl Connector for EchoConnector {
    fn provider(&self) -> &'static str {
        "echo"
    }
    fn action_schemas(&self) -> Vec<ActionSchema> {
        vec![echo_schema()]
    }
}

/// Schéma de l'unique action.
pub fn echo_schema() -> ActionSchema {
    ActionSchema {
        provider: "echo",
        kind: "echo.say",
        label: "Echo — Renvoyer un message",
        description: "Envoie un payload qui est renvoyé tel quel.",
        params: vec![ParamSpec::req_str("message", "Message à renvoyer")],
    }
}

/// Action `POST /v1/echo` — renvoie ce qu'on lui envoie.
pub struct EchoAction<C: HttpClient> {
    auth: EchoAuth,
    client: C,
}

impl<C: HttpClient> EchoAction<C> {
    /// Construit l'action.
    pub fn new(auth: EchoAuth, client: C) -> Self {
        Self { auth, client }
    }
}

impl<C: HttpClient> Action for EchoAction<C> {
    fn kind(&self) -> &'static str {
        "echo.say"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("echo say : `message` requis".into()))?;
        let req = HttpRequest {
            method: "POST".into(),
            url: format!("{ECHO_API}/echo"),
            headers: headers_for(&self.auth, true),
            body: Some(json!({ "message": message })),
        };
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("echo say : {e}")))?;
        interpret(resp, &EchoInterpreter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::RecordingClient;
    use serde_json::json;

    #[test]
    fn echo_sends_post_with_message_and_api_key() {
        let act = EchoAction::new(
            EchoAuth::ApiKey("k-1".into()),
            RecordingClient::ok(json!({ "echoed": "hi" })),
        );
        let out = act.execute(&json!({ "message": "hi" })).unwrap();
        let sent = act.client.last().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.url, "https://echo.example.invalid/v1/echo");
        assert_eq!(
            sent.headers.get("X-Api-Key").map(String::as_str),
            Some("k-1")
        );
        assert_eq!(
            sent.headers.get("Content-Type").map(String::as_str),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(sent.body.unwrap(), json!({ "message": "hi" }));
        assert_eq!(out["echoed"], json!("hi"));
    }

    #[test]
    fn echo_requires_message() {
        let act = EchoAction::new(EchoAuth::ApiKey("k".into()), RecordingClient::ok(json!({})));
        let err = act.execute(&json!({})).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("message")));
    }

    #[test]
    fn echo_maps_error_response() {
        let act = EchoAction::new(
            EchoAuth::ApiKey("k".into()),
            RecordingClient::with_status(400, json!({ "error": "bad request" })),
        );
        let err = act.execute(&json!({ "message": "hi" })).unwrap_err();
        match err {
            Error::Invalid(m) => {
                assert!(m.contains("echo: http 400"), "msg={m}");
                assert!(m.contains("bad request"), "msg={m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn connector_publishes_one_schema() {
        let c = EchoConnector;
        assert_eq!(c.provider(), "echo");
        let schemas = c.action_schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].kind, "echo.say");
    }
}

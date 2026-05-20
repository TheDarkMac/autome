//! Interprétation des réponses HTTP des API connecteurs (F4.5.b).
//!
//! Chaque provider a un format d'erreur légèrement différent ; le SDK
//! mutualise la *structure* du mapping (status HTTP → `Error::Invalid`)
//! et laisse au provider la charge d'extraire le détail texte via
//! [`ResponseInterpreter`].
//!
//! ```ignore
//! struct SlackInterpreter;
//! impl ResponseInterpreter for SlackInterpreter {
//!     fn provider(&self) -> &'static str { "slack" }
//!     fn on_success(&self, body: Value) -> Result<Value> {
//!         // Slack répond souvent 200 mais avec ok=false
//!         if body.get("ok") == Some(&Value::Bool(false)) {
//!             let code = body.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
//!             return Err(Error::Invalid(format!("slack: {code}")));
//!         }
//!         Ok(body)
//!     }
//! }
//! ```

use autome_core::{Error, Result};
use serde_json::Value;

use crate::http::HttpResponse;

/// Stratégie d'interprétation des réponses d'une API.
pub trait ResponseInterpreter {
    /// Identifiant du provider, préfixé dans les messages d'erreur
    /// (ex. `"slack"`, `"gmail"`).
    fn provider(&self) -> &'static str;

    /// Hook appelé pour toute réponse `2xx`. L'impl peut transformer ou
    /// rejeter le body (cas Slack `ok:false` malgré HTTP 200). Par défaut :
    /// renvoyer le body tel quel.
    fn on_success(&self, body: Value) -> Result<Value> {
        Ok(body)
    }

    /// Hook appelé pour toute réponse non-`2xx`. Retourne `Some(detail)`
    /// extrait du body (ex. `error.message` Google, `message` GitHub) ou
    /// `None` pour laisser le SDK utiliser le body brut comme détail.
    #[allow(unused_variables)]
    fn extract_error(&self, body: &Value) -> Option<String> {
        None
    }
}

/// Traduit une [`HttpResponse`] en `Result<Value>` en consultant l'interpréteur.
pub fn interpret<I: ResponseInterpreter>(resp: HttpResponse, interpreter: &I) -> Result<Value> {
    if (200..300).contains(&resp.status) {
        return interpreter.on_success(resp.body);
    }
    let detail = interpreter
        .extract_error(&resp.body)
        .unwrap_or_else(|| resp.body.to_string());
    Err(Error::Invalid(format!(
        "{}: http {} — {}",
        interpreter.provider(),
        resp.status,
        detail
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Plain;
    impl ResponseInterpreter for Plain {
        fn provider(&self) -> &'static str {
            "plain"
        }
    }

    struct WithError;
    impl ResponseInterpreter for WithError {
        fn provider(&self) -> &'static str {
            "x"
        }
        fn extract_error(&self, body: &Value) -> Option<String> {
            body.get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }
    }

    struct PostSuccess;
    impl ResponseInterpreter for PostSuccess {
        fn provider(&self) -> &'static str {
            "y"
        }
        fn on_success(&self, body: Value) -> Result<Value> {
            if body.get("ok") == Some(&Value::Bool(false)) {
                let code = body
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                return Err(Error::Invalid(format!("y: {code}")));
            }
            Ok(body)
        }
    }

    #[test]
    fn two_hundred_returns_body() {
        let out = interpret(
            HttpResponse {
                status: 200,
                body: json!({ "a": 1 }),
            },
            &Plain,
        )
        .unwrap();
        assert_eq!(out, json!({ "a": 1 }));
    }

    #[test]
    fn non_2xx_uses_raw_body_when_no_extractor() {
        let err = interpret(
            HttpResponse {
                status: 500,
                body: json!({ "boom": true }),
            },
            &Plain,
        )
        .unwrap_err();
        match err {
            Error::Invalid(m) => {
                assert!(m.contains("plain: http 500"), "msg={m}");
                assert!(m.contains("boom"), "msg={m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn non_2xx_uses_extracted_detail() {
        let err = interpret(
            HttpResponse {
                status: 400,
                body: json!({ "error": { "message": "Bad arg" } }),
            },
            &WithError,
        )
        .unwrap_err();
        match err {
            Error::Invalid(m) => {
                assert!(m.contains("x: http 400"), "msg={m}");
                assert!(m.contains("Bad arg"), "msg={m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn on_success_can_reject() {
        let err = interpret(
            HttpResponse {
                status: 200,
                body: json!({ "ok": false, "code": "bad" }),
            },
            &PostSuccess,
        )
        .unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("y: bad"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}

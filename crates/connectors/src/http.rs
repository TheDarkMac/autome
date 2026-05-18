//! Action HTTP générique (F3.6) — requête REST configurable.
//!
//! [`HttpAction`] implémente [`autome_core::Action`] : elle traduit les
//! `params` d'un node en [`HttpRequest`] et délègue l'envoi à un
//! [`HttpClient`] **injecté** (seam). Aucun client réseau n'est embarqué
//! ici (pas de dépendance `reqwest`) : un client réel sera branché au
//! câblage (domaine 4/7), et les tests utilisent un client factice.
//!
//! Schéma des `params` :
//! ```json
//! { "method": "POST", "url": "https://api/x",
//!   "headers": { "Authorization": "Bearer …" }, "body": { … } }
//! ```
//! `method` par défaut `GET` ; `url` obligatoire.

use std::collections::BTreeMap;

use autome_core::{Action, Error, Result};
use serde_json::Value;

/// Requête HTTP construite depuis les `params` du node.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    /// Méthode HTTP en majuscules (`GET`, `POST`, …).
    pub method: String,
    /// URL cible.
    pub url: String,
    /// En-têtes (ordonnés pour un comportement déterministe).
    pub headers: BTreeMap<String, String>,
    /// Corps de requête (le cas échéant).
    pub body: Option<Value>,
}

/// Réponse HTTP renvoyée par un [`HttpClient`].
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    /// Code de statut HTTP.
    pub status: u16,
    /// Corps de réponse décodé en JSON.
    pub body: Value,
}

/// Transport HTTP injectable (réel en prod, factice en test).
pub trait HttpClient {
    /// Envoie `req` et renvoie la réponse, ou un message d'erreur.
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String>;
}

/// Action HTTP générique paramétrable.
pub struct HttpAction<C: HttpClient> {
    client: C,
}

impl<C: HttpClient> HttpAction<C> {
    /// Crée l'action avec le client de transport fourni.
    pub fn new(client: C) -> Self {
        Self { client }
    }

    fn build_request(params: &Value) -> Result<HttpRequest> {
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("action http : `url` requis".into()))?
            .to_string();
        let method = params
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_uppercase();
        let mut headers = BTreeMap::new();
        if let Some(obj) = params.get("headers").and_then(Value::as_object) {
            for (k, v) in obj {
                let val = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                headers.insert(k.clone(), val);
            }
        }
        let body = params.get("body").cloned();
        Ok(HttpRequest {
            method,
            url,
            headers,
            body,
        })
    }
}

impl<C: HttpClient> Action for HttpAction<C> {
    fn kind(&self) -> &'static str {
        "http_request"
    }

    fn execute(&self, params: &Value) -> Result<Value> {
        let req = Self::build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("action http : {e}")))?;
        Ok(serde_json::json!({
            "status": resp.status,
            "body": resp.body,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Client factice : enregistre la dernière requête, renvoie une
    /// réponse canned (ou une erreur si `fail`).
    struct MockClient {
        last: RefCell<Option<HttpRequest>>,
        fail: bool,
    }
    impl HttpClient for MockClient {
        fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
            *self.last.borrow_mut() = Some(req.clone());
            if self.fail {
                Err("connexion refusée".into())
            } else {
                Ok(HttpResponse {
                    status: 200,
                    body: serde_json::json!({ "ok": true }),
                })
            }
        }
    }

    #[test]
    fn builds_request_from_params_and_maps_response() {
        let client = MockClient {
            last: RefCell::new(None),
            fail: false,
        };
        let action = HttpAction::new(client);
        assert_eq!(action.kind(), "http_request");

        let out = action
            .execute(&serde_json::json!({
                "method": "post",
                "url": "https://api/x",
                "headers": { "X-Key": "abc" },
                "body": { "a": 1 }
            }))
            .unwrap();

        assert_eq!(
            out,
            serde_json::json!({ "status": 200, "body": { "ok": true } })
        );
        let sent = action.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "POST"); // normalisé en majuscules
        assert_eq!(sent.url, "https://api/x");
        assert_eq!(sent.headers.get("X-Key").unwrap(), "abc");
        assert_eq!(sent.body, Some(serde_json::json!({ "a": 1 })));
    }

    #[test]
    fn defaults_to_get_when_method_absent() {
        let action = HttpAction::new(MockClient {
            last: RefCell::new(None),
            fail: false,
        });
        action
            .execute(&serde_json::json!({ "url": "https://api/y" }))
            .unwrap();
        assert_eq!(action.client.last.borrow().as_ref().unwrap().method, "GET");
    }

    #[test]
    fn missing_url_is_an_error() {
        let action = HttpAction::new(MockClient {
            last: RefCell::new(None),
            fail: false,
        });
        let err = action
            .execute(&serde_json::json!({ "method": "GET" }))
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[test]
    fn transport_error_is_propagated() {
        let action = HttpAction::new(MockClient {
            last: RefCell::new(None),
            fail: true,
        });
        let err = action
            .execute(&serde_json::json!({ "url": "https://api/z" }))
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }
}

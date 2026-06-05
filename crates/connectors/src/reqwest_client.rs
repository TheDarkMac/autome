//! Transport HTTP de **production** (`reqwest::blocking`) — dette #107.
//!
//! Le trait [`HttpClient`] est **synchrone** ; les actions des connecteurs
//! sont exécutées par le worker *thread-based*. On utilise donc le client
//! **bloquant** de `reqwest` (TLS rustls), construit **paresseusement** :
//! `reqwest::blocking::Client` instancie un runtime interne, ce qui paniquerait
//! s'il était construit **dans** le runtime tokio de l'API. En différant la
//! construction au premier `send`, on garantit qu'elle a lieu :
//! - sur un thread du pool worker (hors runtime), ou
//! - dans un `spawn_blocking` côté API (cf. handler OAuth callback).
//!
//! Sémantique du corps : un `body` `Value::String` est envoyé **tel quel**
//! (cas `application/x-www-form-urlencoded` de l'échange OAuth), sinon le
//! `Value` est sérialisé en JSON.

use std::sync::{Arc, OnceLock};

use serde_json::Value;

use crate::http::{HttpClient, HttpRequest, HttpResponse};

/// Client HTTP réel, bloquant, à TLS rustls. Réutilisable et **clonable** : le
/// client `reqwest` sous-jacent (paresseux) est partagé via `Arc<OnceLock>`,
/// donc cloner ne reconstruit pas le runtime interne.
#[derive(Default, Clone)]
pub struct ReqwestHttpClient {
    inner: Arc<OnceLock<reqwest::blocking::Client>>,
}

impl ReqwestHttpClient {
    /// Crée un client (la connexion sous-jacente est paresseuse).
    pub fn new() -> Self {
        Self::default()
    }

    fn client(&self) -> &reqwest::blocking::Client {
        self.inner.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .build()
                .expect("construction client reqwest")
        })
    }
}

impl HttpClient for ReqwestHttpClient {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| format!("méthode HTTP invalide `{}` : {e}", req.method))?;
        let mut rb = self.client().request(method, &req.url);
        for (k, v) in &req.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some(body) = &req.body {
            let bytes = match body {
                // Corps pré-encodé (ex. form-urlencoded OAuth) : tel quel.
                Value::String(s) => s.clone().into_bytes(),
                // Sinon : JSON.
                other => {
                    serde_json::to_vec(other).map_err(|e| format!("sérialisation corps : {e}"))?
                }
            };
            rb = rb.body(bytes);
        }
        let resp = rb.send().map_err(|e| format!("transport : {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().map_err(|e| format!("lecture du corps : {e}"))?;
        // Corps interprété en JSON ; à défaut, exposé comme chaîne brute.
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Test d'intégration **réseau**, gaté par `TEST_HTTP_URL` (skip sinon).
    /// Ex. : `TEST_HTTP_URL=https://httpbin.org/post cargo test -p autome-connectors --test ...`
    /// Ici on vérifie surtout que l'aller-retour ne panique pas hors runtime.
    #[test]
    fn real_get_when_url_provided() {
        let Some(url) = std::env::var("TEST_HTTP_URL").ok() else {
            eprintln!("TEST_HTTP_URL absent → test réseau sauté");
            return;
        };
        let client = ReqwestHttpClient::new();
        let resp = client
            .send(&HttpRequest {
                method: "GET".into(),
                url,
                headers: BTreeMap::new(),
                body: None,
            })
            .expect("réponse");
        assert!((200..500).contains(&resp.status));
    }
}

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
//!
//! **Garde-fou SSRF** (backlog post-audit) : chaque requête passe par une
//! [`EgressPolicy`] — URL contrôlée avant l'envoi, adresses DNS filtrées par
//! un résolveur dédié (la connexion part vers les adresses validées : pas de
//! DNS rebinding), redirections revérifiées. Délais et taille de réponse
//! bornés ([`REQUEST_TIMEOUT`], [`CONNECT_TIMEOUT`], [`MAX_RESPONSE_BYTES`]).
//! Les proxys de l'environnement (`HTTP(S)_PROXY`, `ALL_PROXY`) sont
//! **ignorés** (#233) : derrière un proxy, seul le nom du proxy passerait par
//! le résolveur filtré et l'hôte cible ne serait plus contrôlé.

use std::io::Read;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use serde_json::Value;

use crate::egress::{EgressError, EgressPolicy};
use crate::http::{HttpClient, HttpRequest, HttpResponse};

/// Durée maximale d'une requête complète.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Durée maximale d'établissement de la connexion.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Taille maximale d'un corps de réponse lu (10 Mio) : au-delà, erreur
/// plutôt que de charger en mémoire une réponse arbitrairement grande.
pub const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;
/// Nombre maximal de redirections suivies.
pub const MAX_REDIRECTS: usize = 5;

/// Résolveur DNS qui n'accepte que des adresses autorisées par la politique.
struct GuardedResolver {
    policy: Arc<EgressPolicy>,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = Arc::clone(&self.policy);
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed = policy.check_resolved(&host, addrs)?;
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

/// Client HTTP réel, bloquant, à TLS rustls. Réutilisable et **clonable** : le
/// client `reqwest` sous-jacent (paresseux) est partagé via `Arc<OnceLock>`,
/// donc cloner ne reconstruit pas le runtime interne.
#[derive(Default, Clone)]
pub struct ReqwestHttpClient {
    inner: Arc<OnceLock<reqwest::blocking::Client>>,
    policy: Arc<EgressPolicy>,
}

impl ReqwestHttpClient {
    /// Crée un client à politique **stricte** (destinations publiques
    /// uniquement). La connexion sous-jacente est paresseuse.
    pub fn new() -> Self {
        Self::default()
    }

    /// Crée un client avec la politique fournie (typiquement
    /// [`EgressPolicy::from_env`] pour honorer l'allowlist de l'opérateur).
    pub fn with_policy(policy: EgressPolicy) -> Self {
        Self {
            inner: Arc::default(),
            policy: Arc::new(policy),
        }
    }

    fn client(&self) -> &reqwest::blocking::Client {
        self.inner.get_or_init(|| {
            let policy = Arc::clone(&self.policy);
            let redirects = redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error(EgressError("trop de redirections".into()));
                }
                match policy.check_url(attempt.url()) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(e),
                }
            });
            reqwest::blocking::Client::builder()
                // Pas de proxy : la cible doit passer par `GuardedResolver`.
                .no_proxy()
                .dns_resolver(Arc::new(GuardedResolver {
                    policy: Arc::clone(&self.policy),
                }))
                .redirect(redirects)
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .expect("construction client reqwest")
        })
    }
}

/// Message d'erreur avec sa chaîne de causes (la raison d'un refus egress est
/// enfouie sous les erreurs reqwest/hyper).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !msg.contains(&text) {
            msg.push_str(" : ");
            msg.push_str(&text);
        }
        source = cause.source();
    }
    msg
}

impl HttpClient for ReqwestHttpClient {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| format!("méthode HTTP invalide `{}` : {e}", req.method))?;
        let url = reqwest::Url::parse(&req.url)
            .map_err(|e| format!("URL invalide `{}` : {e}", req.url))?;
        self.policy.check_url(&url).map_err(|e| e.to_string())?;
        let mut rb = self.client().request(method, url);
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
        let resp = rb
            .send()
            .map_err(|e| format!("transport : {}", error_chain(&e)))?;
        let status = resp.status().as_u16();
        let mut raw = Vec::new();
        resp.take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut raw)
            .map_err(|e| format!("lecture du corps : {e}"))?;
        if raw.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(format!(
                "réponse trop volumineuse (> {MAX_RESPONSE_BYTES} octets)"
            ));
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
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
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mini serveur HTTP local : répond `response` à chaque connexion et
    /// compte les connexions reçues. Renvoie `(port, compteur)`.
    fn serve(response: Vec<u8>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&response);
            }
        });
        (port, hits)
    }

    fn ok_json(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn get(client: &ReqwestHttpClient, url: &str) -> std::result::Result<HttpResponse, String> {
        client.send(&HttpRequest {
            method: "GET".into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
        })
    }

    #[test]
    fn strict_client_refuses_loopback_literal_without_connecting() {
        let (port, hits) = serve(ok_json("{}"));
        let err = get(
            &ReqwestHttpClient::new(),
            &format!("http://127.0.0.1:{port}/"),
        )
        .unwrap_err();
        assert!(err.contains("egress refusé"), "{err}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "aucune connexion ne doit partir"
        );
    }

    #[test]
    fn strict_client_refuses_hostname_resolving_to_loopback() {
        let (port, hits) = serve(ok_json("{}"));
        let err = get(
            &ReqwestHttpClient::new(),
            &format!("http://localhost:{port}/"),
        )
        .unwrap_err();
        assert!(err.contains("egress refusé"), "{err}");
        assert!(err.contains("non publique"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn strict_client_refuses_cloud_metadata_and_non_http_schemes() {
        let client = ReqwestHttpClient::new();
        for url in [
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "http://[::ffff:169.254.169.254]/",
            "file:///etc/passwd",
        ] {
            let err = get(&client, url).unwrap_err();
            assert!(err.contains("egress refusé"), "{url} : {err}");
        }
    }

    #[test]
    fn allowlisted_host_is_reachable() {
        let (port, hits) = serve(ok_json(r#"{"ok":true}"#));
        let client =
            ReqwestHttpClient::with_policy(EgressPolicy::with_allowed_hosts(["localhost"]));
        let resp = get(&client, &format!("http://localhost:{port}/")).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, serde_json::json!({ "ok": true }));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn redirect_to_internal_address_is_refused() {
        let redirect = b"HTTP/1.1 302 Found\r\nlocation: http://169.254.169.254/latest/meta-data/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec();
        let (port, _) = serve(redirect);
        let client =
            ReqwestHttpClient::with_policy(EgressPolicy::with_allowed_hosts(["localhost"]));
        let err = get(&client, &format!("http://localhost:{port}/")).unwrap_err();
        assert!(err.contains("egress refusé"), "{err}");
        assert!(err.contains("169.254.169.254"), "{err}");
    }

    #[test]
    fn oversized_response_is_rejected() {
        let size = MAX_RESPONSE_BYTES as usize + 1;
        let mut big =
            format!("HTTP/1.1 200 OK\r\ncontent-length: {size}\r\nconnection: close\r\n\r\n")
                .into_bytes();
        big.resize(big.len() + size, b'a');
        let (port, _) = serve(big);
        let client =
            ReqwestHttpClient::with_policy(EgressPolicy::with_allowed_hosts(["localhost"]));
        let err = get(&client, &format!("http://localhost:{port}/")).unwrap_err();
        assert!(err.contains("trop volumineuse"), "{err}");
    }

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

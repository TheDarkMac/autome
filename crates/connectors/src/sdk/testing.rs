//! Transport factice partagé entre tous les tests connecteurs (F4.5).
//!
//! Remplace les 4 `MockClient` qui s'étaient dupliqués dans `slack.rs`,
//! `gmail.rs`, `notion.rs`, `github.rs`. Deux modes :
//! - [`RecordingClient::ok`] / [`with_status`](Self::with_status) /
//!   [`failing`](Self::failing) : renvoie **toujours** la même réponse ;
//! - [`RecordingClient::queue`] : consomme une FIFO de réponses (utile
//!   pour tester les retries / la pagination).

use std::cell::RefCell;
use std::collections::VecDeque;

use serde_json::Value;

use crate::http::{HttpClient, HttpRequest, HttpResponse};

type SendResult = std::result::Result<HttpResponse, String>;

enum Mode {
    Sticky(SendResult),
    Queue(RefCell<VecDeque<SendResult>>),
}

/// Transport factice qui enregistre toutes les requêtes envoyées.
pub struct RecordingClient {
    sent: RefCell<Vec<HttpRequest>>,
    mode: Mode,
}

impl RecordingClient {
    /// Réponse `200 OK` avec `body`, renvoyée à chaque appel.
    pub fn ok(body: Value) -> Self {
        Self {
            sent: RefCell::new(Vec::new()),
            mode: Mode::Sticky(Ok(HttpResponse { status: 200, body })),
        }
    }

    /// Réponse `status` + `body`, renvoyée à chaque appel.
    pub fn with_status(status: u16, body: Value) -> Self {
        Self {
            sent: RefCell::new(Vec::new()),
            mode: Mode::Sticky(Ok(HttpResponse { status, body })),
        }
    }

    /// Échec de transport renvoyé à chaque appel.
    pub fn failing(msg: &str) -> Self {
        Self {
            sent: RefCell::new(Vec::new()),
            mode: Mode::Sticky(Err(msg.into())),
        }
    }

    /// File FIFO de réponses. Chaque `send` en consomme une. La file ne
    /// doit pas être vide au moment de l'appel — sinon l'appel renvoie
    /// `Err("RecordingClient: queue exhausted")`.
    pub fn queue(responses: Vec<SendResult>) -> Self {
        Self {
            sent: RefCell::new(Vec::new()),
            mode: Mode::Queue(RefCell::new(responses.into())),
        }
    }

    /// Dernière requête envoyée (la plus récente).
    pub fn last(&self) -> Option<HttpRequest> {
        self.sent.borrow().last().cloned()
    }

    /// Nombre de `send` reçus.
    pub fn sent_count(&self) -> usize {
        self.sent.borrow().len()
    }

    /// Toutes les requêtes envoyées, dans l'ordre.
    pub fn all_sent(&self) -> Vec<HttpRequest> {
        self.sent.borrow().clone()
    }
}

impl HttpClient for RecordingClient {
    fn send(&self, req: &HttpRequest) -> SendResult {
        self.sent.borrow_mut().push(req.clone());
        match &self.mode {
            Mode::Sticky(r) => r.clone(),
            Mode::Queue(q) => q
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("RecordingClient: queue exhausted".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req() -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url: "https://x/y".into(),
            headers: Default::default(),
            body: None,
        }
    }

    #[test]
    fn sticky_response_repeats() {
        let c = RecordingClient::ok(json!({ "a": 1 }));
        let r1 = c.send(&req()).unwrap();
        let r2 = c.send(&req()).unwrap();
        assert_eq!(r1.status, 200);
        assert_eq!(r1.body, json!({ "a": 1 }));
        assert_eq!(r2.body, json!({ "a": 1 }));
        assert_eq!(c.sent_count(), 2);
    }

    #[test]
    fn queue_consumes_fifo() {
        let c = RecordingClient::queue(vec![
            Ok(HttpResponse {
                status: 200,
                body: json!({ "n": 1 }),
            }),
            Err("dns".into()),
            Ok(HttpResponse {
                status: 500,
                body: json!({ "n": 2 }),
            }),
        ]);
        assert_eq!(c.send(&req()).unwrap().body, json!({ "n": 1 }));
        assert_eq!(c.send(&req()).unwrap_err(), "dns");
        assert_eq!(c.send(&req()).unwrap().status, 500);
        // Queue épuisée
        assert!(c.send(&req()).is_err());
    }

    #[test]
    fn last_and_all_sent_track_requests() {
        let c = RecordingClient::ok(json!({}));
        let mut r1 = req();
        r1.url = "https://1".into();
        let mut r2 = req();
        r2.url = "https://2".into();
        let _ = c.send(&r1);
        let _ = c.send(&r2);
        assert_eq!(c.last().unwrap().url, "https://2");
        assert_eq!(
            c.all_sent()
                .iter()
                .map(|r| r.url.clone())
                .collect::<Vec<_>>(),
            vec!["https://1", "https://2"]
        );
    }

    #[test]
    fn failing_returns_error_every_time() {
        let c = RecordingClient::failing("boom");
        assert_eq!(c.send(&req()).unwrap_err(), "boom");
        assert_eq!(c.send(&req()).unwrap_err(), "boom");
    }
}

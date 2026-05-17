//! Trigger Webhook (F3.2) — HTTP entrant.
//!
//! Modèle *push* : un appel HTTP entrant (câblé à une route axum au
//! domaine 7/8) appelle [`WebhookTrigger::receive`] ; les événements sont
//! enfilés et restitués via [`autome_core::Trigger::poll`] (F3.1).
//! Ici : l'abstraction + la file FIFO, testables sans serveur.

use std::collections::VecDeque;

use autome_core::{Trigger, TriggerEvent};
use serde_json::Value;

/// Déclencheur sur réception d'une requête HTTP vers un chemin donné.
#[derive(Debug)]
pub struct WebhookTrigger {
    path: String,
    queue: VecDeque<Value>,
}

impl WebhookTrigger {
    /// Crée un trigger écoutant le chemin `path` (ex. `/hooks/abc`).
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            queue: VecDeque::new(),
        }
    }

    /// Chemin écouté.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Notifie une requête entrante. Enfile le `payload` si le chemin
    /// correspond ; renvoie `true` si l'événement a été accepté.
    pub fn receive(&mut self, path: &str, payload: Value) -> bool {
        if path == self.path {
            self.queue.push_back(payload);
            true
        } else {
            false
        }
    }

    /// Nombre d'événements en attente.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }
}

impl Trigger for WebhookTrigger {
    fn kind(&self) -> &'static str {
        "webhook"
    }

    fn poll(&mut self) -> autome_core::Result<Option<TriggerEvent>> {
        Ok(self
            .queue
            .pop_front()
            .map(|payload| TriggerEvent::new("webhook", payload)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_matching_path() {
        let mut t = WebhookTrigger::new("/hooks/a");
        assert_eq!(t.path(), "/hooks/a");
        assert!(!t.receive("/hooks/other", serde_json::json!({})));
        assert!(t.receive("/hooks/a", serde_json::json!({ "x": 1 })));
        assert_eq!(t.pending(), 1);
    }

    #[test]
    fn poll_returns_events_fifo_then_none() {
        let mut t = WebhookTrigger::new("/h");
        t.receive("/h", serde_json::json!({ "n": 1 }));
        t.receive("/h", serde_json::json!({ "n": 2 }));

        let first = t.poll().unwrap().unwrap();
        assert_eq!(first.source, "webhook");
        assert_eq!(first.payload, serde_json::json!({ "n": 1 }));
        assert_eq!(
            t.poll().unwrap().unwrap().payload,
            serde_json::json!({ "n": 2 })
        );
        assert!(t.poll().unwrap().is_none());
    }
}

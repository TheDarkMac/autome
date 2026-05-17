//! Trigger Polling (F3.4) — vérification périodique d'une source.
//!
//! À chaque `poll`, interroge une source injectée (`FnMut() -> Vec<Value>`)
//! et émet un événement par **nouvel** élément, dédupliqué par un champ
//! clé. La cadence réelle d'interrogation est gérée par le planificateur
//! (F5.3) ; ici la seule logique de détection des nouveautés, déterministe
//! et testable sans réseau.

use std::collections::{HashSet, VecDeque};

use autome_core::{Trigger, TriggerEvent};
use serde_json::Value;

/// Déclencheur qui émet sur chaque élément inédit d'une source.
pub struct PollingTrigger<S: FnMut() -> Vec<Value>> {
    source: S,
    /// Champ utilisé comme clé de déduplication (ex. `"id"`).
    key: String,
    seen: HashSet<String>,
    queue: VecDeque<Value>,
}

impl<S: FnMut() -> Vec<Value>> PollingTrigger<S> {
    /// Crée un trigger dédupliquant les éléments via leur champ `key`.
    /// Si un élément n'a pas ce champ, l'élément entier sert de clé.
    pub fn new(key: impl Into<String>, source: S) -> Self {
        Self {
            source,
            key: key.into(),
            seen: HashSet::new(),
            queue: VecDeque::new(),
        }
    }

    fn dedup_key(&self, item: &Value) -> String {
        match item.get(&self.key) {
            Some(v) => v.to_string(),
            None => item.to_string(),
        }
    }

    fn refill(&mut self) {
        for item in (self.source)() {
            let k = self.dedup_key(&item);
            if self.seen.insert(k) {
                self.queue.push_back(item);
            }
        }
    }
}

impl<S: FnMut() -> Vec<Value>> Trigger for PollingTrigger<S> {
    fn kind(&self) -> &'static str {
        "polling"
    }

    fn poll(&mut self) -> autome_core::Result<Option<TriggerEvent>> {
        if self.queue.is_empty() {
            self.refill();
        }
        Ok(self
            .queue
            .pop_front()
            .map(|payload| TriggerEvent::new("polling", payload)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn emits_only_new_items_by_key() {
        // La source grandit entre les appels.
        let batches = RefCell::new(vec![
            vec![
                serde_json::json!({ "id": 1 }),
                serde_json::json!({ "id": 2 }),
            ],
            vec![
                serde_json::json!({ "id": 2 }), // déjà vu
                serde_json::json!({ "id": 3 }), // nouveau
            ],
        ]);
        let mut call = 0;
        let mut trig = PollingTrigger::new("id", || {
            let b = batches.borrow();
            let out = b.get(call).cloned().unwrap_or_default();
            call += 1;
            out
        });

        // 1er refill : id 1 et 2.
        assert_eq!(
            trig.poll().unwrap().unwrap().payload,
            serde_json::json!({ "id": 1 })
        );
        assert_eq!(
            trig.poll().unwrap().unwrap().payload,
            serde_json::json!({ "id": 2 })
        );
        // File vide → 2e refill : seul id 3 est neuf.
        let ev = trig.poll().unwrap().unwrap();
        assert_eq!(ev.source, "polling");
        assert_eq!(ev.payload, serde_json::json!({ "id": 3 }));
        // Plus rien (3e refill vide).
        assert!(trig.poll().unwrap().is_none());
        assert_eq!(trig.kind(), "polling");
    }

    #[test]
    fn falls_back_to_whole_value_when_key_absent() {
        let mut once = false;
        let mut trig = PollingTrigger::new("id", || {
            if once {
                vec![]
            } else {
                once = true;
                vec![serde_json::json!({ "x": 1 }), serde_json::json!({ "x": 1 })]
            }
        });
        // Deux éléments identiques sans champ `id` → un seul émis.
        assert!(trig.poll().unwrap().is_some());
        assert!(trig.poll().unwrap().is_none());
    }
}

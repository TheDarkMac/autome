//! Échéances des déclencheurs planifiés (#227) : `schedule` (cron /
//! intervalle) et `polling`.
//!
//! Une **ligne par nœud déclencheur** `(propriétaire, workflow, nœud)` porte
//! le type et les paramètres du nœud, sa prochaine échéance, un état libre (les
//! clés déjà vues d'un polling) et une **révision**. Le store est partagé par
//! toutes les répliques du worker : chacune lit les échéances atteintes
//! ([`TriggerScheduleStore::due`]) et tente de les **avancer** par
//! compare-and-set sur la révision ([`TriggerScheduleStore::advance`]). Une
//! seule réussit pour une révision donnée : c'est elle qui déclenche. Pas
//! d'élection de leader, pas de verrou tenu pendant le déclenchement.
//!
//! L'API tient la table à jour à chaque enregistrement d'un workflow
//! ([`TriggerScheduleStore::sync`]) ; un nœud dont le type ou les paramètres
//! changent repart de zéro (échéance à recalculer, état vidé). Le calcul des
//! échéances (cron, intervalle) vit dans `autome_connectors::triggers` : ce
//! module reste agnostique.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use crate::{Error, Result};

/// Nœud déclencheur planifiable d'un workflow, tel qu'enregistré.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledTrigger {
    /// Nœud trigger.
    pub node_id: String,
    /// Type du trigger (`"schedule"` ou `"polling"`).
    pub trigger_type: String,
    /// Paramètres du nœud (cron, intervalle, URL…).
    pub params: Value,
}

/// Échéance d'un nœud déclencheur, lue par la boucle de planification.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleSlot {
    /// Propriétaire du workflow — et des runs déclenchés.
    pub owner_id: String,
    /// Workflow dans l'espace du propriétaire.
    pub workflow_id: String,
    /// Nœud trigger.
    pub node_id: String,
    /// Type du trigger.
    pub trigger_type: String,
    /// Paramètres du nœud.
    pub params: Value,
    /// Prochaine échéance (ms Unix) ; `None` = à calculer (nœud nouveau ou
    /// modifié).
    pub next_fire_ms: Option<i64>,
    /// État propre au type (polling : clés déjà vues). `{}` au départ.
    pub state: Value,
    /// Révision, incrémentée à chaque modification de la ligne.
    pub revision: i64,
}

/// Nouvelles valeurs d'une échéance, appliquées par
/// [`TriggerScheduleStore::advance`].
#[derive(Debug, Clone, PartialEq)]
pub struct SlotUpdate {
    /// Prochaine échéance.
    pub next_fire_ms: Option<i64>,
    /// Nouvel état.
    pub state: Value,
    /// Paramètres inutilisables : le nœud n'est plus planifié jusqu'à sa
    /// prochaine modification (l'API valide à l'enregistrement ; ne concerne
    /// que des données antérieures).
    pub error: Option<String>,
}

/// Stockage partagé des échéances.
#[async_trait]
pub trait TriggerScheduleStore: Send + Sync {
    /// Aligne les échéances du workflow sur ses nœuds planifiables
    /// `triggers` : nœuds absents supprimés, nouveaux créés, ceux dont le
    /// type ou les paramètres changent remis à zéro (échéance `None`, état
    /// `{}`, erreur effacée, révision incrémentée) ; les autres sont
    /// inchangés. `triggers` vide = plus aucune échéance (workflow supprimé).
    async fn sync(
        &self,
        owner: &str,
        workflow_id: &str,
        triggers: &[ScheduledTrigger],
    ) -> Result<()>;

    /// Au plus `limit` échéances à traiter : à calculer (`None`, en premier)
    /// ou atteintes (`≤ now_ms`, les plus anciennes d'abord), hors nœuds en
    /// erreur.
    async fn due(&self, now_ms: i64, limit: usize) -> Result<Vec<ScheduleSlot>>;

    /// Applique `update` si la ligne est toujours à la révision
    /// `slot.revision` (qui passe alors à `slot.revision + 1`) ; `false` si
    /// elle a changé entre-temps (autre réplique, workflow modifié ou
    /// supprimé) — l'appelant abandonne alors ce déclenchement.
    async fn advance(&self, slot: &ScheduleSlot, update: SlotUpdate) -> Result<bool>;
}

type Key = (String, String, String);

#[derive(Debug, Clone)]
struct Row {
    trigger_type: String,
    params: Value,
    next_fire_ms: Option<i64>,
    state: Value,
    error: Option<String>,
    revision: i64,
}

/// Implémentation en mémoire (tests, mono-processus).
#[derive(Debug, Default)]
pub struct InMemoryTriggerScheduleStore {
    rows: Mutex<BTreeMap<Key, Row>>,
}

impl InMemoryTriggerScheduleStore {
    /// Store vide.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<Key, Row>>> {
        self.rows
            .lock()
            .map_err(|_| Error::Invalid("échéances des triggers : verrou empoisonné".into()))
    }
}

#[async_trait]
impl TriggerScheduleStore for InMemoryTriggerScheduleStore {
    async fn sync(
        &self,
        owner: &str,
        workflow_id: &str,
        triggers: &[ScheduledTrigger],
    ) -> Result<()> {
        let mut rows = self.lock()?;
        rows.retain(|(o, w, n), _| {
            o != owner || w != workflow_id || triggers.iter().any(|t| &t.node_id == n)
        });
        for t in triggers {
            let key = (
                owner.to_string(),
                workflow_id.to_string(),
                t.node_id.clone(),
            );
            match rows.get_mut(&key) {
                Some(row) if row.trigger_type == t.trigger_type && row.params == t.params => {}
                Some(row) => {
                    *row = Row {
                        trigger_type: t.trigger_type.clone(),
                        params: t.params.clone(),
                        next_fire_ms: None,
                        state: Value::Object(Default::default()),
                        error: None,
                        revision: row.revision + 1,
                    };
                }
                None => {
                    rows.insert(
                        key,
                        Row {
                            trigger_type: t.trigger_type.clone(),
                            params: t.params.clone(),
                            next_fire_ms: None,
                            state: Value::Object(Default::default()),
                            error: None,
                            revision: 0,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    async fn due(&self, now_ms: i64, limit: usize) -> Result<Vec<ScheduleSlot>> {
        let rows = self.lock()?;
        let mut due: Vec<ScheduleSlot> = rows
            .iter()
            .filter(|(_, r)| r.error.is_none() && r.next_fire_ms.is_none_or(|t| t <= now_ms))
            .map(|((owner, workflow, node), r)| ScheduleSlot {
                owner_id: owner.clone(),
                workflow_id: workflow.clone(),
                node_id: node.clone(),
                trigger_type: r.trigger_type.clone(),
                params: r.params.clone(),
                next_fire_ms: r.next_fire_ms,
                state: r.state.clone(),
                revision: r.revision,
            })
            .collect();
        // `None` d'abord, puis par échéance croissante.
        due.sort_by_key(|s| s.next_fire_ms.map_or((0, 0), |t| (1, t)));
        due.truncate(limit);
        Ok(due)
    }

    async fn advance(&self, slot: &ScheduleSlot, update: SlotUpdate) -> Result<bool> {
        let mut rows = self.lock()?;
        let key = (
            slot.owner_id.clone(),
            slot.workflow_id.clone(),
            slot.node_id.clone(),
        );
        match rows.get_mut(&key) {
            Some(row) if row.revision == slot.revision => {
                row.next_fire_ms = update.next_fire_ms;
                row.state = update.state;
                row.error = update.error;
                row.revision += 1;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cron(node: &str, expr: &str) -> ScheduledTrigger {
        ScheduledTrigger {
            node_id: node.into(),
            trigger_type: "schedule".into(),
            params: json!({ "cron": expr }),
        }
    }

    fn update(next: i64) -> SlotUpdate {
        SlotUpdate {
            next_fire_ms: Some(next),
            state: json!({}),
            error: None,
        }
    }

    #[tokio::test]
    async fn new_triggers_are_due_immediately_to_be_armed() {
        let store = InMemoryTriggerScheduleStore::new();
        store
            .sync("alice", "wf", &[cron("t", "* * * * *")])
            .await
            .unwrap();
        let due = store.due(0, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].owner_id, "alice");
        assert_eq!(due[0].next_fire_ms, None);
        assert_eq!(due[0].state, json!({}));
    }

    #[tokio::test]
    async fn advance_is_a_compare_and_set_on_the_revision() {
        let store = InMemoryTriggerScheduleStore::new();
        store
            .sync("alice", "wf", &[cron("t", "* * * * *")])
            .await
            .unwrap();
        let slot = store.due(0, 10).await.unwrap().remove(0);
        // Deux répliques lisent la même révision : une seule l'avance.
        assert!(store.advance(&slot, update(60_000)).await.unwrap());
        assert!(!store.advance(&slot, update(60_000)).await.unwrap());
        // Pas encore atteinte, puis atteinte.
        assert!(store.due(59_999, 10).await.unwrap().is_empty());
        let next = store.due(60_000, 10).await.unwrap().remove(0);
        assert_eq!(next.revision, slot.revision + 1);
        assert_eq!(next.next_fire_ms, Some(60_000));
    }

    #[tokio::test]
    async fn sync_keeps_unchanged_triggers_and_resets_modified_ones() {
        let store = InMemoryTriggerScheduleStore::new();
        store
            .sync(
                "alice",
                "wf",
                &[cron("a", "0 * * * *"), cron("b", "0 * * * *")],
            )
            .await
            .unwrap();
        for slot in store.due(0, 10).await.unwrap() {
            store
                .advance(
                    &slot,
                    SlotUpdate {
                        state: json!({ "seen": ["x"] }),
                        ..update(3_600_000)
                    },
                )
                .await
                .unwrap();
        }
        let before = store.due(3_600_000, 10).await.unwrap();

        // `a` inchangé, `b` modifié, `c` ajouté.
        store
            .sync(
                "alice",
                "wf",
                &[
                    cron("a", "0 * * * *"),
                    cron("b", "30 * * * *"),
                    cron("c", "* * * * *"),
                ],
            )
            .await
            .unwrap();
        let after = store.due(3_600_000, 10).await.unwrap();
        let get = |node: &str| after.iter().find(|s| s.node_id == node).unwrap().clone();
        assert_eq!(
            get("a"),
            before.iter().find(|s| s.node_id == "a").cloned().unwrap()
        );
        let b = get("b");
        assert_eq!((b.next_fire_ms, b.state.clone()), (None, json!({})));
        // L'ancienne révision de `b` ne peut plus être avancée.
        let stale = before.iter().find(|s| s.node_id == "b").unwrap();
        assert!(!store.advance(stale, update(1)).await.unwrap());
        assert_eq!(get("c").next_fire_ms, None);
    }

    #[tokio::test]
    async fn sync_removes_absent_nodes_and_leaves_other_workflows_alone() {
        let store = InMemoryTriggerScheduleStore::new();
        store
            .sync("alice", "wf", &[cron("t", "* * * * *")])
            .await
            .unwrap();
        store
            .sync("alice", "other", &[cron("t", "* * * * *")])
            .await
            .unwrap();
        store
            .sync("bob", "wf", &[cron("t", "* * * * *")])
            .await
            .unwrap();
        store.sync("alice", "wf", &[]).await.unwrap();
        let mut left: Vec<_> = store
            .due(0, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.owner_id, s.workflow_id))
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                ("alice".into(), "other".into()),
                ("bob".into(), "wf".into())
            ]
        );
    }

    #[tokio::test]
    async fn due_orders_unarmed_first_then_oldest_and_skips_errors() {
        let store = InMemoryTriggerScheduleStore::new();
        store
            .sync(
                "alice",
                "wf",
                &[
                    cron("late", "x"),
                    cron("early", "x"),
                    cron("new", "x"),
                    cron("bad", "x"),
                ],
            )
            .await
            .unwrap();
        for slot in store.due(0, 10).await.unwrap() {
            let up = match slot.node_id.as_str() {
                "late" => update(200),
                "early" => update(100),
                "bad" => SlotUpdate {
                    error: Some("cron invalide".into()),
                    next_fire_ms: None,
                    state: json!({}),
                },
                _ => continue,
            };
            store.advance(&slot, up).await.unwrap();
        }
        let order: Vec<_> = store
            .due(1_000, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.node_id)
            .collect();
        assert_eq!(order, ["new", "early", "late"]);
        assert_eq!(store.due(1_000, 2).await.unwrap().len(), 2);
    }
}

//! Persistance des workflows (F7.1.a).
//!
//! Le trait [`WorkflowRepository`] est consommé par l'API publique
//! (F7.1.b/c). L'impl de référence [`InMemoryWorkflowRepository`] suffit
//! pour tests + dev ; l'impl PG/sqlx contre la table `workflows` (F1.5)
//! viendra dans une PR ultérieure.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::workflow::{WorkflowGraph, WorkflowId};
use crate::{Error, Result};

/// Critères de pagination pour [`WorkflowRepository::list`].
#[derive(Debug, Clone, Copy)]
pub struct Pagination {
    /// Offset (combien d'entrées sauter, depuis le début).
    pub offset: usize,
    /// Limite (combien d'entrées retourner au max).
    pub limit: usize,
}

impl Pagination {
    /// Pagination par défaut : offset 0, limit 50.
    pub const fn default_paging() -> Self {
        Self {
            offset: 0,
            limit: 50,
        }
    }
}

impl Default for Pagination {
    fn default() -> Self {
        Self::default_paging()
    }
}

/// Résultat d'une opération de liste.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowPage {
    /// Items de la page (tri stable par `id`).
    pub items: Vec<WorkflowGraph>,
    /// Nombre **total** d'entrées (hors pagination), pour calculer le
    /// nombre de pages côté UI.
    pub total: usize,
}

/// Contrat de persistance d'un workflow.
pub trait WorkflowRepository {
    /// Crée un workflow. `Err` si l'`id` existe déjà.
    fn create(&self, workflow: WorkflowGraph) -> Result<()>;

    /// Charge un workflow par id. `Ok(None)` si inconnu.
    fn get(&self, id: &WorkflowId) -> Result<Option<WorkflowGraph>>;

    /// Liste paginée, ordre stable par `WorkflowId`.
    fn list(&self, paging: Pagination) -> Result<WorkflowPage>;

    /// Met à jour un workflow existant. `Err` si inconnu.
    /// L'`id` du `WorkflowGraph` doit correspondre.
    fn update(&self, workflow: WorkflowGraph) -> Result<()>;

    /// Supprime un workflow. `Ok(false)` si inconnu (idempotent côté caller).
    fn delete(&self, id: &WorkflowId) -> Result<bool>;
}

/// Impl in-process, thread-safe (`Mutex<HashMap>`). Pour tests + dev.
#[derive(Debug, Default)]
pub struct InMemoryWorkflowRepository {
    inner: Mutex<HashMap<String, WorkflowGraph>>,
}

impl InMemoryWorkflowRepository {
    /// Crée un repo vide.
    pub fn new() -> Self {
        Self::default()
    }
}

impl WorkflowRepository for InMemoryWorkflowRepository {
    fn create(&self, workflow: WorkflowGraph) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?;
        if map.contains_key(&workflow.id.0) {
            return Err(Error::Invalid(format!(
                "workflow : id '{}' déjà existant",
                workflow.id.0
            )));
        }
        map.insert(workflow.id.0.clone(), workflow);
        Ok(())
    }

    fn get(&self, id: &WorkflowId) -> Result<Option<WorkflowGraph>> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?
            .get(&id.0)
            .cloned())
    }

    fn list(&self, paging: Pagination) -> Result<WorkflowPage> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?;
        let total = map.len();
        let mut all: Vec<WorkflowGraph> = map.values().cloned().collect();
        // Tri stable par id pour des résultats reproductibles.
        all.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        let items = all
            .into_iter()
            .skip(paging.offset)
            .take(paging.limit)
            .collect();
        Ok(WorkflowPage { items, total })
    }

    fn update(&self, workflow: WorkflowGraph) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?;
        if !map.contains_key(&workflow.id.0) {
            return Err(Error::Invalid(format!(
                "workflow : id '{}' inconnu",
                workflow.id.0
            )));
        }
        map.insert(workflow.id.0.clone(), workflow);
        Ok(())
    }

    fn delete(&self, id: &WorkflowId) -> Result<bool> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?;
        Ok(map.remove(&id.0).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{Node, NodeKind};
    use crate::NodeId;

    fn sample(id: &str) -> WorkflowGraph {
        WorkflowGraph {
            id: WorkflowId::new(id),
            name: format!("workflow {id}"),
            nodes: vec![Node {
                id: NodeId::new("entry"),
                name: "entry".into(),
                kind: NodeKind::Trigger {
                    trigger_type: "manual".into(),
                },
                params: serde_json::Value::Null,
            }],
            edges: vec![],
        }
    }

    #[test]
    fn create_then_get_round_trips() {
        let repo = InMemoryWorkflowRepository::new();
        let w = sample("wf-1");
        repo.create(w.clone()).unwrap();
        let loaded = repo.get(&WorkflowId::new("wf-1")).unwrap().unwrap();
        assert_eq!(loaded, w);
    }

    #[test]
    fn get_unknown_returns_none() {
        let repo = InMemoryWorkflowRepository::new();
        assert!(repo.get(&WorkflowId::new("nope")).unwrap().is_none());
    }

    #[test]
    fn duplicate_create_errors() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("wf-1")).unwrap();
        let err = repo.create(sample("wf-1")).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("déjà")));
    }

    #[test]
    fn list_returns_all_sorted() {
        let repo = InMemoryWorkflowRepository::new();
        for id in ["zeta", "alpha", "mu"] {
            repo.create(sample(id)).unwrap();
        }
        let page = repo.list(Pagination::default()).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.items.len(), 3);
        let ids: Vec<&str> = page.items.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn list_pagination_skips_and_limits() {
        let repo = InMemoryWorkflowRepository::new();
        for i in 0..10 {
            repo.create(sample(&format!("wf-{i:02}"))).unwrap();
        }
        let page = repo
            .list(Pagination {
                offset: 3,
                limit: 4,
            })
            .unwrap();
        assert_eq!(page.total, 10, "total ignore pagination");
        assert_eq!(page.items.len(), 4);
        let ids: Vec<&str> = page.items.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, vec!["wf-03", "wf-04", "wf-05", "wf-06"]);
    }

    #[test]
    fn list_with_offset_beyond_total_returns_empty() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("only")).unwrap();
        let page = repo
            .list(Pagination {
                offset: 100,
                limit: 50,
            })
            .unwrap();
        assert_eq!(page.total, 1);
        assert!(page.items.is_empty());
    }

    #[test]
    fn update_replaces_existing() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("wf-1")).unwrap();
        let mut modified = sample("wf-1");
        modified.name = "renamed".into();
        repo.update(modified.clone()).unwrap();
        let loaded = repo.get(&WorkflowId::new("wf-1")).unwrap().unwrap();
        assert_eq!(loaded.name, "renamed");
    }

    #[test]
    fn update_unknown_errors() {
        let repo = InMemoryWorkflowRepository::new();
        let err = repo.update(sample("nope")).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    #[test]
    fn delete_existing_returns_true() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("wf-1")).unwrap();
        assert!(repo.delete(&WorkflowId::new("wf-1")).unwrap());
        assert!(repo.get(&WorkflowId::new("wf-1")).unwrap().is_none());
    }

    #[test]
    fn delete_unknown_returns_false() {
        let repo = InMemoryWorkflowRepository::new();
        assert!(!repo.delete(&WorkflowId::new("nope")).unwrap());
    }

    #[test]
    fn repo_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryWorkflowRepository>();
    }
}

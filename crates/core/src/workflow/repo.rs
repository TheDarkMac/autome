//! Persistance des workflows (F7.1.a).
//!
//! Le trait [`WorkflowRepository`] est consommé par l'API publique
//! (F7.1.b/c). L'impl de référence [`InMemoryWorkflowRepository`] suffit
//! pour tests + dev ; l'impl PG vit dans `autome-persistence`.
//!
//! **Propriétaire** (backlog BOLA) : chaque workflow est rattaché au
//! `claims.sub` qui l'a créé. Le propriétaire n'est **pas** dans le
//! [`WorkflowGraph`] (document soumis par le client, qui ne doit pas pouvoir
//! le choisir) : le repository le stocke à côté, le pose à la création et ne
//! le modifie jamais. L'enforcement (404 si non-propriétaire) vit dans l'API.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::workflow::{WorkflowGraph, WorkflowId};
use crate::{Error, Result};
use async_trait::async_trait;

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
#[async_trait]
pub trait WorkflowRepository: Send + Sync {
    /// Crée un workflow appartenant à `owner` (`None` = sans propriétaire,
    /// réservé aux outils internes/tests). `Err` si l'`id` existe déjà.
    async fn create(&self, workflow: WorkflowGraph, owner: Option<&str>) -> Result<()>;

    /// Charge un workflow par id. `Ok(None)` si inconnu. **Sans contrôle
    /// d'accès** : réservé au worker et aux handlers qui ont déjà vérifié
    /// [`Self::owner_of`].
    async fn get(&self, id: &WorkflowId) -> Result<Option<WorkflowGraph>>;

    /// Propriétaire du workflow. `Ok(None)` si le workflow est inconnu **ou**
    /// sans propriétaire — dans les deux cas, inaccessible à un utilisateur
    /// ordinaire.
    async fn owner_of(&self, id: &WorkflowId) -> Result<Option<String>>;

    /// Liste paginée, ordre stable par `WorkflowId`. `owner = Some(u)`
    /// restreint aux workflows de `u` (`total` compris) ; `None` = tous.
    async fn list(&self, owner: Option<&str>, paging: Pagination) -> Result<WorkflowPage>;

    /// Met à jour un workflow existant. `Err` si inconnu.
    /// L'`id` du `WorkflowGraph` doit correspondre.
    async fn update(&self, workflow: WorkflowGraph) -> Result<()>;

    /// Supprime un workflow. `Ok(false)` si inconnu (idempotent côté caller).
    async fn delete(&self, id: &WorkflowId) -> Result<bool>;
}

/// Workflow stocké + son propriétaire.
#[derive(Debug, Clone)]
struct Stored {
    graph: WorkflowGraph,
    owner: Option<String>,
}

/// Impl in-process, thread-safe (`Mutex<HashMap>`). Pour tests + dev.
#[derive(Debug, Default)]
pub struct InMemoryWorkflowRepository {
    inner: Mutex<HashMap<String, Stored>>,
}

impl InMemoryWorkflowRepository {
    /// Crée un repo vide.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WorkflowRepository for InMemoryWorkflowRepository {
    async fn create(&self, workflow: WorkflowGraph, owner: Option<&str>) -> Result<()> {
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
        map.insert(
            workflow.id.0.clone(),
            Stored {
                graph: workflow,
                owner: owner.map(str::to_string),
            },
        );
        Ok(())
    }

    async fn get(&self, id: &WorkflowId) -> Result<Option<WorkflowGraph>> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?
            .get(&id.0)
            .map(|s| s.graph.clone()))
    }

    async fn owner_of(&self, id: &WorkflowId) -> Result<Option<String>> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?
            .get(&id.0)
            .and_then(|s| s.owner.clone()))
    }

    async fn list(&self, owner: Option<&str>, paging: Pagination) -> Result<WorkflowPage> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?;
        let mut all: Vec<WorkflowGraph> = map
            .values()
            .filter(|s| owner.is_none() || s.owner.as_deref() == owner)
            .map(|s| s.graph.clone())
            .collect();
        let total = all.len();
        // Tri stable par id pour des résultats reproductibles.
        all.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        let items = all
            .into_iter()
            .skip(paging.offset)
            .take(paging.limit)
            .collect();
        Ok(WorkflowPage { items, total })
    }

    async fn update(&self, workflow: WorkflowGraph) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))?;
        // Le propriétaire est conservé : seul le graphe est remplacé.
        let Some(stored) = map.get_mut(&workflow.id.0) else {
            return Err(Error::Invalid(format!(
                "workflow : id '{}' inconnu",
                workflow.id.0
            )));
        };
        stored.graph = workflow;
        Ok(())
    }

    async fn delete(&self, id: &WorkflowId) -> Result<bool> {
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

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let repo = InMemoryWorkflowRepository::new();
        let w = sample("wf-1");
        repo.create(w.clone(), None).await.unwrap();
        let loaded = repo.get(&WorkflowId::new("wf-1")).await.unwrap().unwrap();
        assert_eq!(loaded, w);
    }

    #[tokio::test]
    async fn get_unknown_returns_none() {
        let repo = InMemoryWorkflowRepository::new();
        assert!(repo.get(&WorkflowId::new("nope")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_create_errors() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("wf-1"), None).await.unwrap();
        let err = repo.create(sample("wf-1"), None).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("déjà")));
    }

    #[tokio::test]
    async fn list_returns_all_sorted() {
        let repo = InMemoryWorkflowRepository::new();
        for id in ["zeta", "alpha", "mu"] {
            repo.create(sample(id), None).await.unwrap();
        }
        let page = repo.list(None, Pagination::default()).await.unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.items.len(), 3);
        let ids: Vec<&str> = page.items.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
    }

    #[tokio::test]
    async fn list_pagination_skips_and_limits() {
        let repo = InMemoryWorkflowRepository::new();
        for i in 0..10 {
            repo.create(sample(&format!("wf-{i:02}")), None)
                .await
                .unwrap();
        }
        let page = repo
            .list(
                None,
                Pagination {
                    offset: 3,
                    limit: 4,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 10, "total ignore pagination");
        assert_eq!(page.items.len(), 4);
        let ids: Vec<&str> = page.items.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, vec!["wf-03", "wf-04", "wf-05", "wf-06"]);
    }

    #[tokio::test]
    async fn list_with_offset_beyond_total_returns_empty() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("only"), None).await.unwrap();
        let page = repo
            .list(
                None,
                Pagination {
                    offset: 100,
                    limit: 50,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert!(page.items.is_empty());
    }

    #[tokio::test]
    async fn update_replaces_existing() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("wf-1"), None).await.unwrap();
        let mut modified = sample("wf-1");
        modified.name = "renamed".into();
        repo.update(modified.clone()).await.unwrap();
        let loaded = repo.get(&WorkflowId::new("wf-1")).await.unwrap().unwrap();
        assert_eq!(loaded.name, "renamed");
    }

    #[tokio::test]
    async fn update_unknown_errors() {
        let repo = InMemoryWorkflowRepository::new();
        let err = repo.update(sample("nope")).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    #[tokio::test]
    async fn delete_existing_returns_true() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("wf-1"), None).await.unwrap();
        assert!(repo.delete(&WorkflowId::new("wf-1")).await.unwrap());
        assert!(repo.get(&WorkflowId::new("wf-1")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_unknown_returns_false() {
        let repo = InMemoryWorkflowRepository::new();
        assert!(!repo.delete(&WorkflowId::new("nope")).await.unwrap());
    }

    #[tokio::test]
    async fn repo_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryWorkflowRepository>();
    }

    // ───── Propriétaire (backlog BOLA) ─────

    #[tokio::test]
    async fn owner_is_recorded_and_listing_is_scoped() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("a-1"), Some("alice")).await.unwrap();
        repo.create(sample("a-2"), Some("alice")).await.unwrap();
        repo.create(sample("b-1"), Some("bob")).await.unwrap();
        repo.create(sample("legacy"), None).await.unwrap();

        assert_eq!(
            repo.owner_of(&WorkflowId::new("a-1"))
                .await
                .unwrap()
                .as_deref(),
            Some("alice")
        );
        assert_eq!(
            repo.owner_of(&WorkflowId::new("legacy")).await.unwrap(),
            None
        );
        assert_eq!(repo.owner_of(&WorkflowId::new("nope")).await.unwrap(), None);

        let alice = repo
            .list(Some("alice"), Pagination::default())
            .await
            .unwrap();
        assert_eq!(alice.total, 2);
        let ids: Vec<&str> = alice.items.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, vec!["a-1", "a-2"]);
        let all = repo.list(None, Pagination::default()).await.unwrap();
        assert_eq!(all.total, 4);
    }

    #[tokio::test]
    async fn update_keeps_the_owner() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(sample("wf"), Some("alice")).await.unwrap();
        let mut renamed = sample("wf");
        renamed.name = "renamed".into();
        repo.update(renamed).await.unwrap();
        assert_eq!(
            repo.owner_of(&WorkflowId::new("wf"))
                .await
                .unwrap()
                .as_deref(),
            Some("alice")
        );
    }
}

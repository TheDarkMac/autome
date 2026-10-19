//! Persistance des workflows (F7.1.a).
//!
//! Le trait [`WorkflowRepository`] est consommé par l'API publique
//! (F7.1.b/c) et le worker. L'impl de référence [`InMemoryWorkflowRepository`]
//! suffit pour tests + dev ; l'impl PG vit dans `autome-persistence`.
//!
//! **Espaces de noms par propriétaire** (#188) : l'identité d'un workflow est
//! le couple `(owner, id)`. Chaque utilisateur (`claims.sub`) a son propre
//! espace d'ids : deux utilisateurs peuvent créer chacun un workflow `wf-draft`
//! sans se voir ni se gêner, et un id pris par autrui ne répond jamais 409.
//! Le propriétaire n'est **pas** dans le [`WorkflowGraph`] (document soumis
//! par le client, qui ne doit pas pouvoir le choisir) : le repository le
//! reçoit de l'appelant (API : l'utilisateur authentifié ; worker : le
//! propriétaire du run) et ne le modifie jamais.
//!
//! Les workflows créés avant la migration `0010` n'ont pas de propriétaire :
//! ils vivent dans l'espace [`LEGACY_NAMESPACE`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::workflow::{WorkflowGraph, WorkflowId};
use crate::{Error, Result};
use async_trait::async_trait;

/// Espace des workflows sans propriétaire (antérieurs à la migration `0010`),
/// réservé aux super-admins. Aucun `claims.sub` n'est vide.
pub const LEGACY_NAMESPACE: &str = "";

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

/// Filtre de [`WorkflowRepository::list_all`] (#243).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowFilter {
    /// Espace exact (`Some("")` = [`LEGACY_NAMESPACE`]) ; `None` = tous.
    pub owner: Option<String>,
    /// Sous-chaîne cherchée dans l'id ou le nom, sans tenir compte de la
    /// casse ; `None` (ou vide) = pas de recherche.
    pub query: Option<String>,
}

impl WorkflowFilter {
    /// Recherche normalisée (minuscules), `None` si absente ou vide.
    pub fn needle(&self) -> Option<String> {
        self.query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase)
    }

    /// `true` si le workflow `(owner, graph)` passe le filtre.
    pub fn matches(&self, owner: &str, graph: &WorkflowGraph) -> bool {
        if self.owner.as_deref().is_some_and(|o| o != owner) {
            return false;
        }
        self.needle().is_none_or(|q| {
            graph.id.0.to_lowercase().contains(&q) || graph.name.to_lowercase().contains(&q)
        })
    }
}

/// Résumé d'un workflow pour la liste d'administration (#243) : son espace
/// et de quoi le reconnaître, sans le graphe (paramètres des nœuds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowListing {
    /// Propriétaire (espace d'ids) ; [`LEGACY_NAMESPACE`] si aucun.
    pub owner: String,
    /// Id dans l'espace du propriétaire.
    pub id: WorkflowId,
    /// Nom affiché.
    pub name: String,
    /// Nombre de nœuds de la version courante.
    pub node_count: usize,
}

/// Page de [`WorkflowRepository::list_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowListingPage {
    /// Résumés de la page, triés par `(owner, id)`.
    pub items: Vec<WorkflowListing>,
    /// Nombre total d'entrées correspondant au filtre (hors pagination).
    pub total: usize,
}

/// Contrat de persistance d'un workflow. Toutes les opérations portent sur
/// l'espace d'ids de `owner` : un workflow d'un autre espace n'existe pas
/// pour elles — sauf [`Self::list_all`], la vue d'administration. Aucun
/// contrôle d'accès ici — l'appelant choisit l'espace (API : celui de
/// l'utilisateur authentifié).
#[async_trait]
pub trait WorkflowRepository: Send + Sync {
    /// Crée un workflow dans l'espace de `owner`. `Err` (message contenant
    /// « déjà ») si l'`id` existe déjà **dans cet espace**.
    async fn create(&self, owner: &str, workflow: WorkflowGraph) -> Result<()>;

    /// Charge le workflow `id` de l'espace de `owner`. `Ok(None)` si inconnu.
    async fn get(&self, owner: &str, id: &WorkflowId) -> Result<Option<WorkflowGraph>>;

    /// Liste paginée de l'espace de `owner`, ordre stable par `WorkflowId`.
    async fn list(&self, owner: &str, paging: Pagination) -> Result<WorkflowPage>;

    /// Met à jour un workflow existant de l'espace de `owner`. `Err` si
    /// inconnu. L'`id` du `WorkflowGraph` désigne le workflow.
    async fn update(&self, owner: &str, workflow: WorkflowGraph) -> Result<()>;

    /// Supprime un workflow de l'espace de `owner`. `Ok(false)` si inconnu
    /// (idempotent côté caller).
    async fn delete(&self, owner: &str, id: &WorkflowId) -> Result<bool>;

    /// Liste d'administration (#243) : workflows de **tous** les espaces
    /// correspondant à `filter`, triés par `(owner, id)`, paginés. Réservée
    /// aux super-admins par l'appelant.
    async fn list_all(
        &self,
        filter: &WorkflowFilter,
        paging: Pagination,
    ) -> Result<WorkflowListingPage>;
}

/// Clé de stockage : `(propriétaire, id)`. `BTreeMap` ⇒ itération triée par
/// propriétaire puis par id.
type Key = (String, String);

fn key(owner: &str, id: &str) -> Key {
    (owner.to_string(), id.to_string())
}

/// Impl in-process, thread-safe (`Mutex<BTreeMap>`). Pour tests + dev.
#[derive(Debug, Default)]
pub struct InMemoryWorkflowRepository {
    inner: Mutex<BTreeMap<Key, WorkflowGraph>>,
}

impl InMemoryWorkflowRepository {
    /// Crée un repo vide.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<Key, WorkflowGraph>>> {
        self.inner
            .lock()
            .map_err(|_| Error::Invalid("workflow repo lock empoisonné".into()))
    }
}

#[async_trait]
impl WorkflowRepository for InMemoryWorkflowRepository {
    async fn create(&self, owner: &str, workflow: WorkflowGraph) -> Result<()> {
        let mut map = self.lock()?;
        let k = key(owner, &workflow.id.0);
        if map.contains_key(&k) {
            return Err(Error::Invalid(format!(
                "workflow : id '{}' déjà existant",
                workflow.id.0
            )));
        }
        map.insert(k, workflow);
        Ok(())
    }

    async fn get(&self, owner: &str, id: &WorkflowId) -> Result<Option<WorkflowGraph>> {
        Ok(self.lock()?.get(&key(owner, &id.0)).cloned())
    }

    async fn list(&self, owner: &str, paging: Pagination) -> Result<WorkflowPage> {
        let map = self.lock()?;
        let mine = map.iter().filter(|((o, _), _)| o == owner);
        let total = mine.clone().count();
        let items = mine
            .skip(paging.offset)
            .take(paging.limit)
            .map(|(_, graph)| graph.clone())
            .collect();
        Ok(WorkflowPage { items, total })
    }

    async fn update(&self, owner: &str, workflow: WorkflowGraph) -> Result<()> {
        let mut map = self.lock()?;
        let Some(stored) = map.get_mut(&key(owner, &workflow.id.0)) else {
            return Err(Error::Invalid(format!(
                "workflow : id '{}' inconnu",
                workflow.id.0
            )));
        };
        *stored = workflow;
        Ok(())
    }

    async fn delete(&self, owner: &str, id: &WorkflowId) -> Result<bool> {
        Ok(self.lock()?.remove(&key(owner, &id.0)).is_some())
    }

    async fn list_all(
        &self,
        filter: &WorkflowFilter,
        paging: Pagination,
    ) -> Result<WorkflowListingPage> {
        let map = self.lock()?;
        let matching = map
            .iter()
            .filter(|((owner, _), graph)| filter.matches(owner, graph));
        let total = matching.clone().count();
        let items = matching
            .skip(paging.offset)
            .take(paging.limit)
            .map(|((owner, _), graph)| WorkflowListing {
                owner: owner.clone(),
                id: graph.id.clone(),
                name: graph.name.clone(),
                node_count: graph.nodes.len(),
            })
            .collect();
        Ok(WorkflowListingPage { items, total })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{Node, NodeKind};
    use crate::NodeId;

    const ME: &str = "alice";

    fn sample(id: &str) -> WorkflowGraph {
        WorkflowGraph {
            id: WorkflowId::new(id),
            name: format!("workflow {id}"),
            nodes: vec![Node {
                position: None,
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

    fn wid(id: &str) -> WorkflowId {
        WorkflowId::new(id)
    }

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let repo = InMemoryWorkflowRepository::new();
        let w = sample("wf-1");
        repo.create(ME, w.clone()).await.unwrap();
        let loaded = repo.get(ME, &wid("wf-1")).await.unwrap().unwrap();
        assert_eq!(loaded, w);
    }

    #[tokio::test]
    async fn get_unknown_returns_none() {
        let repo = InMemoryWorkflowRepository::new();
        assert!(repo.get(ME, &wid("nope")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_create_in_the_same_namespace_errors() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(ME, sample("wf-1")).await.unwrap();
        let err = repo.create(ME, sample("wf-1")).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("déjà")));
    }

    #[tokio::test]
    async fn list_returns_all_sorted() {
        let repo = InMemoryWorkflowRepository::new();
        for id in ["zeta", "alpha", "mu"] {
            repo.create(ME, sample(id)).await.unwrap();
        }
        let page = repo.list(ME, Pagination::default()).await.unwrap();
        assert_eq!(page.total, 3);
        let ids: Vec<&str> = page.items.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
    }

    #[tokio::test]
    async fn list_pagination_skips_and_limits() {
        let repo = InMemoryWorkflowRepository::new();
        for i in 0..10 {
            repo.create(ME, sample(&format!("wf-{i:02}")))
                .await
                .unwrap();
        }
        let page = repo
            .list(
                ME,
                Pagination {
                    offset: 3,
                    limit: 4,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 10, "total ignore pagination");
        let ids: Vec<&str> = page.items.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, vec!["wf-03", "wf-04", "wf-05", "wf-06"]);
    }

    #[tokio::test]
    async fn list_with_offset_beyond_total_returns_empty() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(ME, sample("only")).await.unwrap();
        let page = repo
            .list(
                ME,
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
        repo.create(ME, sample("wf-1")).await.unwrap();
        let mut modified = sample("wf-1");
        modified.name = "renamed".into();
        repo.update(ME, modified).await.unwrap();
        let loaded = repo.get(ME, &wid("wf-1")).await.unwrap().unwrap();
        assert_eq!(loaded.name, "renamed");
    }

    #[tokio::test]
    async fn update_unknown_errors() {
        let repo = InMemoryWorkflowRepository::new();
        let err = repo.update(ME, sample("nope")).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    #[tokio::test]
    async fn delete_existing_returns_true() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create(ME, sample("wf-1")).await.unwrap();
        assert!(repo.delete(ME, &wid("wf-1")).await.unwrap());
        assert!(repo.get(ME, &wid("wf-1")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_unknown_returns_false() {
        let repo = InMemoryWorkflowRepository::new();
        assert!(!repo.delete(ME, &wid("nope")).await.unwrap());
    }

    #[tokio::test]
    async fn repo_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryWorkflowRepository>();
    }

    // ───── Espaces de noms (#188) ─────

    #[tokio::test]
    async fn same_id_lives_independently_in_each_namespace() {
        let repo = InMemoryWorkflowRepository::new();
        let mut a = sample("wf-draft");
        a.name = "chez alice".into();
        let mut b = sample("wf-draft");
        b.name = "chez bob".into();
        repo.create("alice", a).await.unwrap();
        repo.create("bob", b).await.unwrap();

        let get = |o: &'static str| {
            let repo = &repo;
            async move { repo.get(o, &wid("wf-draft")).await.unwrap().unwrap().name }
        };
        assert_eq!(get("alice").await, "chez alice");
        assert_eq!(get("bob").await, "chez bob");

        let mut renamed = sample("wf-draft");
        renamed.name = "alice v2".into();
        repo.update("alice", renamed).await.unwrap();
        assert_eq!(get("bob").await, "chez bob", "l'autre espace est intact");

        assert!(repo.delete("bob", &wid("wf-draft")).await.unwrap());
        assert_eq!(get("alice").await, "alice v2");
        assert!(repo.get("bob", &wid("wf-draft")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn listing_and_lookups_are_confined_to_the_namespace() {
        let repo = InMemoryWorkflowRepository::new();
        repo.create("alice", sample("a-1")).await.unwrap();
        repo.create("alice", sample("a-2")).await.unwrap();
        repo.create("bob", sample("b-1")).await.unwrap();
        repo.create(LEGACY_NAMESPACE, sample("legacy"))
            .await
            .unwrap();

        let alice = repo.list("alice", Pagination::default()).await.unwrap();
        assert_eq!(alice.total, 2);
        let ids: Vec<&str> = alice.items.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, vec!["a-1", "a-2"]);
        let legacy = repo
            .list(LEGACY_NAMESPACE, Pagination::default())
            .await
            .unwrap();
        assert_eq!(legacy.total, 1);

        assert!(repo.get("alice", &wid("b-1")).await.unwrap().is_none());
        assert!(repo.update("alice", sample("b-1")).await.is_err());
        assert!(!repo.delete("alice", &wid("b-1")).await.unwrap());
        assert!(repo.get("bob", &wid("b-1")).await.unwrap().is_some());
    }

    // ───── Liste d'administration (#243) ─────

    async fn three_namespaces() -> InMemoryWorkflowRepository {
        let repo = InMemoryWorkflowRepository::new();
        let mut invoices = sample("wf-b");
        invoices.name = "Factures mensuelles".into();
        repo.create("bob", invoices).await.unwrap();
        repo.create("alice", sample("wf-z")).await.unwrap();
        repo.create("alice", sample("wf-a")).await.unwrap();
        repo.create(LEGACY_NAMESPACE, sample("old")).await.unwrap();
        repo
    }

    fn listed(page: &WorkflowListingPage) -> Vec<(String, String)> {
        page.items
            .iter()
            .map(|w| (w.owner.clone(), w.id.0.clone()))
            .collect()
    }

    fn pair(owner: &str, id: &str) -> (String, String) {
        (owner.to_string(), id.to_string())
    }

    #[tokio::test]
    async fn list_all_spans_every_namespace_sorted_by_owner_then_id() {
        let repo = three_namespaces().await;
        let page = repo
            .list_all(&WorkflowFilter::default(), Pagination::default())
            .await
            .unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(
            listed(&page),
            vec![
                pair("", "old"),
                pair("alice", "wf-a"),
                pair("alice", "wf-z"),
                pair("bob", "wf-b"),
            ]
        );
        let bob = &page.items[3];
        assert_eq!(bob.name, "Factures mensuelles");
        assert_eq!(bob.node_count, 1);
    }

    #[tokio::test]
    async fn list_all_filters_by_exact_owner_including_legacy() {
        let repo = three_namespaces().await;
        let only = |owner: &str| WorkflowFilter {
            owner: Some(owner.into()),
            query: None,
        };
        let alice = repo
            .list_all(&only("alice"), Pagination::default())
            .await
            .unwrap();
        assert_eq!(
            listed(&alice),
            vec![pair("alice", "wf-a"), pair("alice", "wf-z")]
        );
        let legacy = repo
            .list_all(&only(LEGACY_NAMESPACE), Pagination::default())
            .await
            .unwrap();
        assert_eq!(listed(&legacy), vec![pair("", "old")]);
        let prefix = repo
            .list_all(&only("ali"), Pagination::default())
            .await
            .unwrap();
        assert_eq!(prefix.total, 0, "propriétaire exact, pas un préfixe");
    }

    #[tokio::test]
    async fn list_all_searches_id_or_name_case_insensitively() {
        let repo = three_namespaces().await;
        let search = |q: &str| WorkflowFilter {
            owner: None,
            query: Some(q.into()),
        };
        let by_name = repo
            .list_all(&search("  FACTURES "), Pagination::default())
            .await
            .unwrap();
        assert_eq!(listed(&by_name), vec![pair("bob", "wf-b")]);
        let by_id = repo
            .list_all(&search("WF-"), Pagination::default())
            .await
            .unwrap();
        assert_eq!(by_id.total, 3);
        let blank = repo
            .list_all(&search("  "), Pagination::default())
            .await
            .unwrap();
        assert_eq!(blank.total, 4, "recherche vide = pas de recherche");
    }

    #[tokio::test]
    async fn list_all_paginates_with_the_filtered_total() {
        let repo = three_namespaces().await;
        let page = repo
            .list_all(
                &WorkflowFilter::default(),
                Pagination {
                    offset: 1,
                    limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(
            listed(&page),
            vec![pair("alice", "wf-a"), pair("alice", "wf-z")]
        );
    }
}

//! Persistance de l'état d'exécution d'un run (F5.4).
//!
//! Permet à un run de **reprendre après crash** d'un worker et de
//! **nettoyer les orphelins** (runs marqués `running` depuis trop
//! longtemps = worker mort en cours).
//!
//! Le contrat reste agnostique du backend : trait [`RunStateStore`] +
//! impl de référence [`InMemoryRunStateStore`]. Un store PostgreSQL/sqlx
//! sera branché ultérieurement contre les tables `runs` / `run_steps`
//! déjà posées par F1.5 (cf. `migrations/0003_runs.sql`).
//!
//! ## Mode d'emploi côté worker (futur)
//!
//! ```ignore
//! // À chaque transition de node, le worker checkpoint :
//! store.checkpoint(&snapshot).await?;
//!
//! // Au démarrage, on cherche les runs orphelins :
//! let cutoff = now_ms - max_run_age_ms;
//! for orphan in store.list_orphans(cutoff).await? {
//!     store.mark_failed(&orphan.run_id, "orphan: worker mort", now_ms).await?;
//! }
//!
//! // Reprise : si un run est interrompu (status=Running au démarrage),
//! // le store renvoie son dernier état → le worker recrée le contexte
//! // et continue à partir de `next_node`.
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::workflow::repo::Pagination;
use crate::{Error, Result};

/// État global d'un run.
///
/// Type *partagé* (cycle de vie côté worker + engine). Déplacé depuis
/// `engine` en F5.4 ; `autome_engine::RunStatus` reste exposé par
/// ré-export pour la rétrocompatibilité.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    /// En file, pas encore pris par un worker.
    Queued,
    /// En cours d'exécution.
    Running,
    /// Terminé avec succès.
    Succeeded,
    /// Terminé en échec.
    Failed,
}

/// Snapshot d'un run à un instant T. Persisté en base à chaque transition
/// de node pour permettre la reprise après crash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunCheckpoint {
    /// Identifiant du run (UUID côté base — string ici pour rester agnostique).
    pub run_id: String,
    /// Identifiant du workflow exécuté.
    pub workflow_id: String,
    /// Version exacte du workflow (pour éviter qu'une modif post-démarrage
    /// ne change le DAG d'un run en cours).
    pub version: i32,
    /// Type de trigger ayant démarré le run (`"webhook"`, `"schedule"`, …).
    pub trigger_type: String,
    /// État courant.
    pub status: RunStatus,
    /// Timestamp Unix (ms) du démarrage effectif (transition `Queued` → `Running`).
    pub started_at_ms: Option<i64>,
    /// Timestamp Unix (ms) de fin (transition vers `Succeeded`/`Failed`).
    pub finished_at_ms: Option<i64>,
    /// Timestamp Unix (ms) de création (insertion initiale du run).
    pub created_at_ms: i64,
    /// Sorties calculées par node, indexées par `node_id`. Permet de
    /// rejouer un run en reprenant à `next_node` sans recalculer ce qui
    /// est déjà fait.
    pub outputs: BTreeMap<String, Value>,
    /// Prochain `node_id` à exécuter. `None` quand `status` est terminal.
    pub next_node: Option<String>,
    /// Message d'erreur en cas de `Failed`.
    pub error: Option<String>,
    /// Propriétaire du run (`claims.sub` du propriétaire du workflow au
    /// moment du dispatch). Seul lui — ou un super-admin — peut le lire.
    /// **Immuable** une fois posé : les stores ignorent un changement
    /// ultérieur. `None` = run sans propriétaire (créé avant l'isolation
    /// par propriétaire), visible des seuls super-admins.
    #[serde(default)]
    pub owner_id: Option<String>,
}

impl RunCheckpoint {
    /// Crée un snapshot vide en état `Queued` (au moment où le scheduler
    /// publie le run, avant qu'un worker ne le prenne).
    pub fn queued(
        run_id: impl Into<String>,
        workflow_id: impl Into<String>,
        version: i32,
        trigger_type: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            workflow_id: workflow_id.into(),
            version,
            trigger_type: trigger_type.into(),
            status: RunStatus::Queued,
            started_at_ms: None,
            finished_at_ms: None,
            created_at_ms,
            outputs: BTreeMap::new(),
            next_node: None,
            error: None,
            owner_id: None,
        }
    }

    /// Builder : pose le propriétaire du run.
    pub fn with_owner(mut self, owner_id: Option<String>) -> Self {
        self.owner_id = owner_id;
        self
    }
}

// ────────────────────── RunStateStore ──────────────────────

/// Contrat de persistance d'état d'exécution.
///
/// **Async** (dette #107) : l'impl PostgreSQL (`runs`/`run_steps`) fait des
/// I/O. L'impl in-memory reste triviale.
#[async_trait]
pub trait RunStateStore: Send + Sync {
    /// Persiste un snapshot — crée si inconnu, écrase sinon (last-write-wins).
    async fn checkpoint(&self, snapshot: &RunCheckpoint) -> Result<()>;

    /// Charge le dernier snapshot d'un run, ou `None`.
    async fn load(&self, run_id: &str) -> Result<Option<RunCheckpoint>>;

    /// Liste les runs **orphelins** : `status == Running` et `started_at_ms`
    /// strictement antérieur à `older_than_ms`. Caller responsable de
    /// calculer le seuil (`now_ms - max_run_age_ms`).
    async fn list_orphans(&self, older_than_ms: i64) -> Result<Vec<RunCheckpoint>>;

    /// Marque un run comme `Failed` avec une raison, et pose `finished_at_ms`.
    /// `Err` si le run est inconnu. Idempotent sur un run déjà `Failed`.
    async fn mark_failed(&self, run_id: &str, reason: &str, now_ms: i64) -> Result<()>;

    /// Liste paginée des runs d'un workflow, triés par `created_at_ms`
    /// décroissant (plus récent d'abord), ordre stabilisé par `run_id` à
    /// égalité de timestamp. Consommé par l'endpoint API
    /// `GET /workflows/{id}/runs` (F7.2.d).
    ///
    /// `owner = Some(u)` restreint aux runs de `u` : un workflow supprimé puis
    /// recréé sous le même id par un autre utilisateur ne doit pas lui
    /// exposer les runs de l'ancien propriétaire. `None` = tous (super-admin).
    async fn list_by_workflow(
        &self,
        workflow_id: &str,
        owner: Option<&str>,
        paging: Pagination,
    ) -> Result<RunListPage>;
}

/// Résultat paginé de [`RunStateStore::list_by_workflow`].
#[derive(Debug, Clone, PartialEq)]
pub struct RunListPage {
    /// Items de la page (snapshot complet, plus récent d'abord).
    pub items: Vec<RunCheckpoint>,
    /// Total **hors pagination** pour le workflow demandé.
    pub total: usize,
}

// ────────────────────── InMemoryRunStateStore ──────────────────────

/// Impl de référence, in-process, thread-safe (`Mutex<HashMap>`).
#[derive(Debug, Default)]
pub struct InMemoryRunStateStore {
    inner: Mutex<HashMap<String, RunCheckpoint>>,
}

impl InMemoryRunStateStore {
    /// Crée un store vide.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RunStateStore for InMemoryRunStateStore {
    async fn checkpoint(&self, snapshot: &RunCheckpoint) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("run_state lock empoisonné".into()))?;
        // Propriétaire immuable (même règle que l'impl PostgreSQL) : un
        // checkpoint ultérieur ne peut ni l'effacer ni le changer.
        let mut snap = snapshot.clone();
        if let Some(existing) = map.get(&snapshot.run_id) {
            if existing.owner_id.is_some() {
                snap.owner_id = existing.owner_id.clone();
            }
        }
        map.insert(snapshot.run_id.clone(), snap);
        Ok(())
    }

    async fn load(&self, run_id: &str) -> Result<Option<RunCheckpoint>> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("run_state lock empoisonné".into()))?;
        Ok(map.get(run_id).cloned())
    }

    async fn list_orphans(&self, older_than_ms: i64) -> Result<Vec<RunCheckpoint>> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("run_state lock empoisonné".into()))?;
        let mut orphans: Vec<RunCheckpoint> = map
            .values()
            .filter(|c| {
                matches!(c.status, RunStatus::Running)
                    && c.started_at_ms.map(|t| t < older_than_ms).unwrap_or(false)
            })
            .cloned()
            .collect();
        // Tri stable pour reproductibilité des tests.
        orphans.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        Ok(orphans)
    }

    async fn mark_failed(&self, run_id: &str, reason: &str, now_ms: i64) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("run_state lock empoisonné".into()))?;
        let snap = map
            .get_mut(run_id)
            .ok_or_else(|| Error::Invalid(format!("mark_failed : run {run_id} inconnu")))?;
        if matches!(snap.status, RunStatus::Failed) {
            return Ok(()); // idempotent
        }
        snap.status = RunStatus::Failed;
        snap.error = Some(reason.to_string());
        snap.finished_at_ms = Some(now_ms);
        Ok(())
    }

    async fn list_by_workflow(
        &self,
        workflow_id: &str,
        owner: Option<&str>,
        paging: Pagination,
    ) -> Result<RunListPage> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("run_state lock empoisonné".into()))?;
        let mut all: Vec<RunCheckpoint> = map
            .values()
            .filter(|c| c.workflow_id == workflow_id)
            .filter(|c| owner.is_none() || c.owner_id.as_deref() == owner)
            .cloned()
            .collect();
        // Tri : created_at_ms desc, puis run_id asc pour stabiliser à
        // égalité de timestamp (cas in-memory où plusieurs runs sont
        // insérés dans la même ms).
        all.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| a.run_id.cmp(&b.run_id))
        });
        let total = all.len();
        let items = all
            .into_iter()
            .skip(paging.offset)
            .take(paging.limit)
            .collect();
        Ok(RunListPage { items, total })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(run_id: &str, status: RunStatus, started_at_ms: Option<i64>) -> RunCheckpoint {
        let mut s = RunCheckpoint::queued(run_id, "wf-1", 1, "webhook", 1000);
        s.status = status;
        s.started_at_ms = started_at_ms;
        s
    }

    // ───── F5.4.a — checkpoint ─────

    #[tokio::test]
    async fn checkpoint_inserts_new_run() {
        let store = InMemoryRunStateStore::new();
        let snap = snapshot("r-1", RunStatus::Queued, None);
        store.checkpoint(&snap).await.unwrap();
        let loaded = store.load("r-1").await.unwrap().unwrap();
        assert_eq!(loaded, snap);
    }

    #[tokio::test]
    async fn checkpoint_overwrites_existing() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&snapshot("r-1", RunStatus::Queued, None))
            .await
            .unwrap();
        store
            .checkpoint(&snapshot("r-1", RunStatus::Running, Some(2000)))
            .await
            .unwrap();
        let loaded = store.load("r-1").await.unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Running);
        assert_eq!(loaded.started_at_ms, Some(2000));
    }

    #[tokio::test]
    async fn checkpoint_persists_outputs_and_next_node() {
        let store = InMemoryRunStateStore::new();
        let mut snap = snapshot("r-1", RunStatus::Running, Some(1500));
        snap.outputs.insert("n1".into(), json!({ "result": 42 }));
        snap.outputs
            .insert("n2".into(), json!({ "items": [1, 2, 3] }));
        snap.next_node = Some("n3".into());
        store.checkpoint(&snap).await.unwrap();
        let loaded = store.load("r-1").await.unwrap().unwrap();
        assert_eq!(loaded.outputs["n1"], json!({ "result": 42 }));
        assert_eq!(loaded.outputs["n2"], json!({ "items": [1, 2, 3] }));
        assert_eq!(loaded.next_node.as_deref(), Some("n3"));
    }

    #[tokio::test]
    async fn checkpoint_roundtrips_through_json() {
        let mut snap = snapshot("r-1", RunStatus::Running, Some(1500));
        snap.outputs.insert("n".into(), json!({ "x": 1 }));
        snap.next_node = Some("next".into());
        let s = serde_json::to_string(&snap).unwrap();
        let back: RunCheckpoint = serde_json::from_str(&s).unwrap();
        assert_eq!(snap, back);
    }

    // ───── F5.4.b — load (reprise) ─────

    #[tokio::test]
    async fn load_unknown_run_returns_none() {
        let store = InMemoryRunStateStore::new();
        assert!(store.load("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_returns_full_snapshot_for_resume() {
        let store = InMemoryRunStateStore::new();
        let mut snap = snapshot("r-1", RunStatus::Running, Some(1500));
        snap.outputs.insert("a".into(), json!("done"));
        snap.next_node = Some("b".into());
        store.checkpoint(&snap).await.unwrap();
        // Simule un crash + redémarrage : on charge depuis le store.
        let resumed = store.load("r-1").await.unwrap().unwrap();
        // Le worker peut reprendre à partir de `next_node` avec les outputs
        // déjà connus.
        assert_eq!(resumed.next_node.as_deref(), Some("b"));
        assert_eq!(resumed.outputs["a"], json!("done"));
        assert_eq!(resumed.status, RunStatus::Running);
    }

    // ───── F5.4.c — list_orphans + mark_failed ─────

    #[tokio::test]
    async fn list_orphans_returns_running_runs_older_than_cutoff() {
        let store = InMemoryRunStateStore::new();
        // Run vieux et running → orphan
        store
            .checkpoint(&snapshot("old", RunStatus::Running, Some(100)))
            .await
            .unwrap();
        // Run récent et running → pas orphan
        store
            .checkpoint(&snapshot("recent", RunStatus::Running, Some(9_000)))
            .await
            .unwrap();
        // Run vieux mais succeeded → pas orphan
        store
            .checkpoint(&snapshot("done", RunStatus::Succeeded, Some(100)))
            .await
            .unwrap();
        // Run queued → pas orphan (started_at = None)
        store
            .checkpoint(&snapshot("q", RunStatus::Queued, None))
            .await
            .unwrap();

        let orphans = store.list_orphans(5_000).await.unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].run_id, "old");
    }

    #[tokio::test]
    async fn list_orphans_uses_strict_inequality() {
        let store = InMemoryRunStateStore::new();
        // Run dont started_at == cutoff → pas orphan (strict)
        store
            .checkpoint(&snapshot("edge", RunStatus::Running, Some(5_000)))
            .await
            .unwrap();
        let orphans = store.list_orphans(5_000).await.unwrap();
        assert!(orphans.is_empty());
    }

    #[tokio::test]
    async fn list_orphans_returns_empty_when_none_match() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&snapshot("a", RunStatus::Queued, None))
            .await
            .unwrap();
        store
            .checkpoint(&snapshot("b", RunStatus::Succeeded, Some(100)))
            .await
            .unwrap();
        assert!(store.list_orphans(1_000_000).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_orphans_is_sorted_by_run_id() {
        let store = InMemoryRunStateStore::new();
        for id in &["zeta", "alpha", "mu"] {
            store
                .checkpoint(&snapshot(id, RunStatus::Running, Some(100)))
                .await
                .unwrap();
        }
        let orphans = store.list_orphans(5_000).await.unwrap();
        let ids: Vec<&str> = orphans.iter().map(|o| o.run_id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
    }

    #[tokio::test]
    async fn mark_failed_transitions_run() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&snapshot("r-1", RunStatus::Running, Some(1500)))
            .await
            .unwrap();
        store.mark_failed("r-1", "worker died", 9999).await.unwrap();
        let loaded = store.load("r-1").await.unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Failed);
        assert_eq!(loaded.error.as_deref(), Some("worker died"));
        assert_eq!(loaded.finished_at_ms, Some(9999));
    }

    #[tokio::test]
    async fn mark_failed_unknown_run_errors() {
        let store = InMemoryRunStateStore::new();
        let err = store.mark_failed("missing", "x", 0).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("missing")));
    }

    #[tokio::test]
    async fn mark_failed_is_idempotent_on_already_failed() {
        let store = InMemoryRunStateStore::new();
        let mut snap = snapshot("r-1", RunStatus::Failed, Some(1500));
        snap.error = Some("original".into());
        snap.finished_at_ms = Some(2000);
        store.checkpoint(&snap).await.unwrap();

        // Re-marquer ne doit pas écraser la raison originale.
        store.mark_failed("r-1", "second pass", 9999).await.unwrap();
        let loaded = store.load("r-1").await.unwrap().unwrap();
        assert_eq!(loaded.error.as_deref(), Some("original"));
        assert_eq!(loaded.finished_at_ms, Some(2000));
    }

    /// Vérifie la composition : `Send + Sync` sur l'impl de référence
    /// (précondition pour le sweep périodique d'orphelins en F5.5/F5.6).
    #[tokio::test]
    async fn in_memory_store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryRunStateStore>();
    }

    // ───── F7.2.d — list_by_workflow ─────

    fn run_with_created(run_id: &str, workflow_id: &str, created_at_ms: i64) -> RunCheckpoint {
        let mut s = RunCheckpoint::queued(run_id, workflow_id, 1, "manual", created_at_ms);
        s.status = RunStatus::Queued;
        s
    }

    #[tokio::test]
    async fn list_by_workflow_filters_by_workflow_id() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&run_with_created("r1", "wf-a", 100))
            .await
            .unwrap();
        store
            .checkpoint(&run_with_created("r2", "wf-b", 200))
            .await
            .unwrap();
        store
            .checkpoint(&run_with_created("r3", "wf-a", 300))
            .await
            .unwrap();
        let page = store
            .list_by_workflow("wf-a", None, Pagination::default())
            .await
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
        for item in &page.items {
            assert_eq!(item.workflow_id, "wf-a");
        }
    }

    #[tokio::test]
    async fn list_by_workflow_orders_by_created_desc() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&run_with_created("old", "wf", 100))
            .await
            .unwrap();
        store
            .checkpoint(&run_with_created("new", "wf", 300))
            .await
            .unwrap();
        store
            .checkpoint(&run_with_created("mid", "wf", 200))
            .await
            .unwrap();
        let page = store
            .list_by_workflow("wf", None, Pagination::default())
            .await
            .unwrap();
        let ids: Vec<&str> = page.items.iter().map(|c| c.run_id.as_str()).collect();
        assert_eq!(ids, vec!["new", "mid", "old"]);
    }

    #[tokio::test]
    async fn list_by_workflow_paginates() {
        let store = InMemoryRunStateStore::new();
        for i in 0..5 {
            // created_at distinct pour ordre stable
            store
                .checkpoint(&run_with_created(&format!("r{i}"), "wf", 1000 - i as i64))
                .await
                .unwrap();
        }
        let page = store
            .list_by_workflow(
                "wf",
                None,
                Pagination {
                    offset: 1,
                    limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 5);
        assert_eq!(page.items.len(), 2);
        // Ordre desc par created → r0 (1000), r1 (999), r2 (998)...
        // offset=1, limit=2 → r1, r2
        let ids: Vec<&str> = page.items.iter().map(|c| c.run_id.as_str()).collect();
        assert_eq!(ids, vec!["r1", "r2"]);
    }

    #[tokio::test]
    async fn list_by_workflow_returns_empty_when_unknown() {
        let store = InMemoryRunStateStore::new();
        let page = store
            .list_by_workflow("nope", None, Pagination::default())
            .await
            .unwrap();
        assert_eq!(page.total, 0);
        assert!(page.items.is_empty());
    }

    // ───── Isolation par propriétaire (backlog BOLA) ─────

    #[tokio::test]
    async fn list_by_workflow_filters_by_owner() {
        let store = InMemoryRunStateStore::new();
        let mine = run_with_created("r-mine", "wf", 200).with_owner(Some("alice".into()));
        let theirs = run_with_created("r-theirs", "wf", 100).with_owner(Some("bob".into()));
        let legacy = run_with_created("r-legacy", "wf", 50);
        for r in [&mine, &theirs, &legacy] {
            store.checkpoint(r).await.unwrap();
        }
        let page = store
            .list_by_workflow("wf", Some("alice"), Pagination::default())
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].run_id, "r-mine");
        let all = store
            .list_by_workflow("wf", None, Pagination::default())
            .await
            .unwrap();
        assert_eq!(all.total, 3, "None = sans filtre (super-admin)");
    }

    #[tokio::test]
    async fn checkpoint_keeps_the_first_owner() {
        let store = InMemoryRunStateStore::new();
        let queued = run_with_created("r", "wf", 1).with_owner(Some("alice".into()));
        store.checkpoint(&queued).await.unwrap();
        // Un checkpoint ultérieur sans propriétaire (ou avec un autre) ne
        // doit pas le changer.
        let mut later = run_with_created("r", "wf", 1);
        later.status = RunStatus::Running;
        store.checkpoint(&later).await.unwrap();
        store
            .checkpoint(&later.clone().with_owner(Some("mallory".into())))
            .await
            .unwrap();
        let loaded = store.load("r").await.unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Running);
        assert_eq!(loaded.owner_id.as_deref(), Some("alice"));
    }

    #[test]
    fn checkpoint_without_owner_field_deserializes() {
        // Snapshots sérialisés avant l'ajout du champ : `owner_id` absent.
        let json = serde_json::json!({
            "run_id": "r", "workflow_id": "wf", "version": 1, "trigger_type": "manual",
            "status": "Queued", "started_at_ms": null, "finished_at_ms": null,
            "created_at_ms": 1, "outputs": {}, "next_node": null, "error": null
        });
        let cp: RunCheckpoint = serde_json::from_value(json).unwrap();
        assert_eq!(cp.owner_id, None);
    }
}

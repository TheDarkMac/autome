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
//! store.checkpoint(&snapshot)?;
//!
//! // Au démarrage, on cherche les runs orphelins :
//! let cutoff = now_ms - max_run_age_ms;
//! for orphan in store.list_orphans(cutoff)? {
//!     store.mark_failed(&orphan.run_id, "orphan: worker mort", now_ms)?;
//! }
//!
//! // Reprise : si un run est interrompu (status=Running au démarrage),
//! // le store renvoie son dernier état → le worker recrée le contexte
//! // et continue à partir de `next_node`.
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
        }
    }
}

// ────────────────────── RunStateStore ──────────────────────

/// Contrat de persistance d'état d'exécution.
///
/// Volontairement minimal et sync. Le store en base réelle (sqlx +
/// `runs`/`run_steps`) viendra dans une PR ultérieure ; les triplets
/// (run_id, status, started_at) sont la matière première.
pub trait RunStateStore {
    /// Persiste un snapshot — crée si inconnu, écrase sinon (last-write-wins).
    fn checkpoint(&self, snapshot: &RunCheckpoint) -> Result<()>;

    /// Charge le dernier snapshot d'un run, ou `None`.
    fn load(&self, run_id: &str) -> Result<Option<RunCheckpoint>>;

    /// Liste les runs **orphelins** : `status == Running` et `started_at_ms`
    /// strictement antérieur à `older_than_ms`. Caller responsable de
    /// calculer le seuil (`now_ms - max_run_age_ms`).
    fn list_orphans(&self, older_than_ms: i64) -> Result<Vec<RunCheckpoint>>;

    /// Marque un run comme `Failed` avec une raison, et pose `finished_at_ms`.
    /// `Err` si le run est inconnu. Idempotent sur un run déjà `Failed`.
    fn mark_failed(&self, run_id: &str, reason: &str, now_ms: i64) -> Result<()>;
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

impl RunStateStore for InMemoryRunStateStore {
    fn checkpoint(&self, snapshot: &RunCheckpoint) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("run_state lock empoisonné".into()))?;
        map.insert(snapshot.run_id.clone(), snapshot.clone());
        Ok(())
    }

    fn load(&self, run_id: &str) -> Result<Option<RunCheckpoint>> {
        let map = self
            .inner
            .lock()
            .map_err(|_| Error::Invalid("run_state lock empoisonné".into()))?;
        Ok(map.get(run_id).cloned())
    }

    fn list_orphans(&self, older_than_ms: i64) -> Result<Vec<RunCheckpoint>> {
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

    fn mark_failed(&self, run_id: &str, reason: &str, now_ms: i64) -> Result<()> {
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

    #[test]
    fn checkpoint_inserts_new_run() {
        let store = InMemoryRunStateStore::new();
        let snap = snapshot("r-1", RunStatus::Queued, None);
        store.checkpoint(&snap).unwrap();
        let loaded = store.load("r-1").unwrap().unwrap();
        assert_eq!(loaded, snap);
    }

    #[test]
    fn checkpoint_overwrites_existing() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&snapshot("r-1", RunStatus::Queued, None))
            .unwrap();
        store
            .checkpoint(&snapshot("r-1", RunStatus::Running, Some(2000)))
            .unwrap();
        let loaded = store.load("r-1").unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Running);
        assert_eq!(loaded.started_at_ms, Some(2000));
    }

    #[test]
    fn checkpoint_persists_outputs_and_next_node() {
        let store = InMemoryRunStateStore::new();
        let mut snap = snapshot("r-1", RunStatus::Running, Some(1500));
        snap.outputs.insert("n1".into(), json!({ "result": 42 }));
        snap.outputs
            .insert("n2".into(), json!({ "items": [1, 2, 3] }));
        snap.next_node = Some("n3".into());
        store.checkpoint(&snap).unwrap();
        let loaded = store.load("r-1").unwrap().unwrap();
        assert_eq!(loaded.outputs["n1"], json!({ "result": 42 }));
        assert_eq!(loaded.outputs["n2"], json!({ "items": [1, 2, 3] }));
        assert_eq!(loaded.next_node.as_deref(), Some("n3"));
    }

    #[test]
    fn checkpoint_roundtrips_through_json() {
        let mut snap = snapshot("r-1", RunStatus::Running, Some(1500));
        snap.outputs.insert("n".into(), json!({ "x": 1 }));
        snap.next_node = Some("next".into());
        let s = serde_json::to_string(&snap).unwrap();
        let back: RunCheckpoint = serde_json::from_str(&s).unwrap();
        assert_eq!(snap, back);
    }

    // ───── F5.4.b — load (reprise) ─────

    #[test]
    fn load_unknown_run_returns_none() {
        let store = InMemoryRunStateStore::new();
        assert!(store.load("missing").unwrap().is_none());
    }

    #[test]
    fn load_returns_full_snapshot_for_resume() {
        let store = InMemoryRunStateStore::new();
        let mut snap = snapshot("r-1", RunStatus::Running, Some(1500));
        snap.outputs.insert("a".into(), json!("done"));
        snap.next_node = Some("b".into());
        store.checkpoint(&snap).unwrap();
        // Simule un crash + redémarrage : on charge depuis le store.
        let resumed = store.load("r-1").unwrap().unwrap();
        // Le worker peut reprendre à partir de `next_node` avec les outputs
        // déjà connus.
        assert_eq!(resumed.next_node.as_deref(), Some("b"));
        assert_eq!(resumed.outputs["a"], json!("done"));
        assert_eq!(resumed.status, RunStatus::Running);
    }

    // ───── F5.4.c — list_orphans + mark_failed ─────

    #[test]
    fn list_orphans_returns_running_runs_older_than_cutoff() {
        let store = InMemoryRunStateStore::new();
        // Run vieux et running → orphan
        store
            .checkpoint(&snapshot("old", RunStatus::Running, Some(100)))
            .unwrap();
        // Run récent et running → pas orphan
        store
            .checkpoint(&snapshot("recent", RunStatus::Running, Some(9_000)))
            .unwrap();
        // Run vieux mais succeeded → pas orphan
        store
            .checkpoint(&snapshot("done", RunStatus::Succeeded, Some(100)))
            .unwrap();
        // Run queued → pas orphan (started_at = None)
        store
            .checkpoint(&snapshot("q", RunStatus::Queued, None))
            .unwrap();

        let orphans = store.list_orphans(5_000).unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].run_id, "old");
    }

    #[test]
    fn list_orphans_uses_strict_inequality() {
        let store = InMemoryRunStateStore::new();
        // Run dont started_at == cutoff → pas orphan (strict)
        store
            .checkpoint(&snapshot("edge", RunStatus::Running, Some(5_000)))
            .unwrap();
        let orphans = store.list_orphans(5_000).unwrap();
        assert!(orphans.is_empty());
    }

    #[test]
    fn list_orphans_returns_empty_when_none_match() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&snapshot("a", RunStatus::Queued, None))
            .unwrap();
        store
            .checkpoint(&snapshot("b", RunStatus::Succeeded, Some(100)))
            .unwrap();
        assert!(store.list_orphans(1_000_000).unwrap().is_empty());
    }

    #[test]
    fn list_orphans_is_sorted_by_run_id() {
        let store = InMemoryRunStateStore::new();
        for id in &["zeta", "alpha", "mu"] {
            store
                .checkpoint(&snapshot(id, RunStatus::Running, Some(100)))
                .unwrap();
        }
        let orphans = store.list_orphans(5_000).unwrap();
        let ids: Vec<&str> = orphans.iter().map(|o| o.run_id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn mark_failed_transitions_run() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&snapshot("r-1", RunStatus::Running, Some(1500)))
            .unwrap();
        store.mark_failed("r-1", "worker died", 9999).unwrap();
        let loaded = store.load("r-1").unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Failed);
        assert_eq!(loaded.error.as_deref(), Some("worker died"));
        assert_eq!(loaded.finished_at_ms, Some(9999));
    }

    #[test]
    fn mark_failed_unknown_run_errors() {
        let store = InMemoryRunStateStore::new();
        let err = store.mark_failed("missing", "x", 0).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("missing")));
    }

    #[test]
    fn mark_failed_is_idempotent_on_already_failed() {
        let store = InMemoryRunStateStore::new();
        let mut snap = snapshot("r-1", RunStatus::Failed, Some(1500));
        snap.error = Some("original".into());
        snap.finished_at_ms = Some(2000);
        store.checkpoint(&snap).unwrap();

        // Re-marquer ne doit pas écraser la raison originale.
        store.mark_failed("r-1", "second pass", 9999).unwrap();
        let loaded = store.load("r-1").unwrap().unwrap();
        assert_eq!(loaded.error.as_deref(), Some("original"));
        assert_eq!(loaded.finished_at_ms, Some(2000));
    }

    /// Vérifie la composition : `Send + Sync` sur l'impl de référence
    /// (précondition pour le sweep périodique d'orphelins en F5.5/F5.6).
    #[test]
    fn in_memory_store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryRunStateStore>();
    }
}

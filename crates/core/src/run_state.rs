//! Persistance de l'état d'exécution d'un run (F5.4).
//!
//! Permet à un run de **reprendre après une interruption** du worker et de
//! **nettoyer les orphelins** (runs marqués `running` depuis trop
//! longtemps).
//!
//! Le contrat reste agnostique du backend : trait [`RunStateStore`] +
//! impl de référence [`InMemoryRunStateStore`] ; l'impl PostgreSQL vit dans
//! `autome-persistence` (tables `runs`, `run_steps`, `run_outbox`).
//!
//! ## Cycle de vie d'un run (#239)
//!
//! ```text
//! enqueue (run queued + message, une transaction : outbox)
//!   → publication du message (dispatcher, sinon relais du worker)
//!   → try_claim (queued → running, tentative + 1)
//!   → record_progress avant / après chaque nœud, heartbeat
//!   → finish (état final)
//!   ↘ requeue (interruption : running → queued, message republié)
//! ```
//!
//! **Tentative** ([`RunCheckpoint::attempt`]) : incrémentée à chaque prise en
//! charge, elle sert de jeton de *fencing*. Progression, battement, remise en
//! file et état final ne s'écrivent que pour la tentative en cours : un worker
//! qui a perdu la main (run repris ailleurs) ne peut plus rien écrire.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::queue::Message;
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

/// Snapshot d'un run à un instant T.
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
    /// Timestamp Unix (ms) du démarrage effectif de la tentative en cours
    /// (transition `Queued` → `Running`).
    pub started_at_ms: Option<i64>,
    /// Timestamp Unix (ms) de fin (transition vers `Succeeded`/`Failed`).
    pub finished_at_ms: Option<i64>,
    /// Timestamp Unix (ms) de création (insertion initiale du run).
    pub created_at_ms: i64,
    /// Sorties calculées par node, indexées par `node_id` (écrites avec
    /// l'état final).
    pub outputs: BTreeMap<String, Value>,
    /// Nœud **en cours** : écrit avant son exécution, effacé une fois son
    /// issue enregistrée (#239). Après une interruption, `Some(n)` signifie
    /// que `n` a pu produire ses effets.
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
    /// Tentative en cours : `0` tant que le run n'a jamais été pris,
    /// incrémentée à chaque prise en charge (jeton de fencing, #239).
    #[serde(default)]
    pub attempt: i32,
    /// Dernier battement du worker qui exécute le run (ms Unix).
    #[serde(default)]
    pub heartbeat_at_ms: Option<i64>,
    /// Point de reprise du moteur, enregistré après chaque nœud. Format
    /// interne au worker : jamais sérialisé dans les réponses de l'API.
    #[serde(skip)]
    pub resume_state: Option<Value>,
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
            attempt: 0,
            heartbeat_at_ms: None,
            resume_state: None,
        }
    }

    /// Builder : pose le propriétaire du run.
    pub fn with_owner(mut self, owner_id: Option<String>) -> Self {
        self.owner_id = owner_id;
        self
    }
}

// ────────────────────── Outbox (#239) ──────────────────────

/// Message de file d'un run, enregistré **avec** le run (pattern Outbox) et
/// gardé jusqu'à sa fin pour pouvoir être republié à chaque reprise.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxMessage {
    /// Run concerné.
    pub run_id: String,
    /// Queue cible.
    pub queue: String,
    /// Génération : `0` à la création, incrémentée à chaque remise en file.
    pub generation: i32,
    /// Message à publier (sa clé d'idempotence suit la génération, cf.
    /// [`outbox_idempotency_key`]).
    pub message: Message,
}

/// Clé d'idempotence du message d'un run pour une génération : le `run_id`
/// seul à la création, suffixé ensuite — le worker qui a traité une
/// génération ne doit pas ignorer la suivante.
pub fn outbox_idempotency_key(run_id: &str, generation: i32) -> String {
    if generation == 0 {
        run_id.to_string()
    } else {
        format!("{run_id}#{generation}")
    }
}

/// Progression d'une tentative, enregistrée par le worker autour de chaque
/// nœud ([`RunStateStore::record_progress`]).
#[derive(Debug, Clone, PartialEq)]
pub enum RunProgress {
    /// `node` va s'exécuter : il devient le nœud en cours (`next_node`).
    Starting {
        /// Nœud qui démarre.
        node: String,
    },
    /// Le dernier nœud est traité : plus de nœud en cours, nouveau point de
    /// reprise.
    Done {
        /// Point de reprise du moteur.
        resume_state: Value,
    },
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

    /// Crée le run et son message de file **ensemble** (une transaction en
    /// PostgreSQL) : ni run sans message, ni message sans run (#239). Le
    /// message reste « à publier » jusqu'à [`mark_published`](Self::mark_published).
    async fn enqueue(&self, snapshot: &RunCheckpoint, message: &OutboxMessage) -> Result<()>;

    /// Messages encore à publier, créés (ou remis en file) avant
    /// `created_before_ms`, plus anciens d'abord, au plus `limit`.
    async fn pending_outbox(
        &self,
        created_before_ms: i64,
        limit: usize,
    ) -> Result<Vec<OutboxMessage>>;

    /// Marque publié le message de `run_id` **de cette génération** ; sans
    /// effet si le message a été remis en file depuis (nouvelle génération)
    /// ou n'existe plus.
    async fn mark_published(&self, run_id: &str, generation: i32, now_ms: i64) -> Result<()>;

    /// Écrit l'état final d'un run **seulement s'il existe encore et que sa
    /// tentative est toujours `snapshot.attempt`** (même écriture que
    /// [`checkpoint`](Self::checkpoint), propriétaire compris) et retire son
    /// message de file ; `false` sinon. Le worker l'utilise après exécution :
    /// un run effacé pendant qu'il tournait (#229) n'est pas recréé, un run
    /// repris par un autre worker n'est pas écrasé (#239).
    async fn finish(&self, snapshot: &RunCheckpoint) -> Result<bool>;

    /// Charge le dernier snapshot d'un run, ou `None`.
    async fn load(&self, run_id: &str) -> Result<Option<RunCheckpoint>>;

    /// Liste les runs **orphelins** : `status == Running` et `started_at_ms`
    /// strictement antérieur à `older_than_ms`. Caller responsable de
    /// calculer le seuil (`now_ms - max_run_age_ms`).
    async fn list_orphans(&self, older_than_ms: i64) -> Result<Vec<RunCheckpoint>>;

    /// Marque un run comme `Failed` avec une raison, et pose `finished_at_ms`.
    /// `Err` si le run est inconnu. Idempotent sur un run déjà `Failed`.
    async fn mark_failed(&self, run_id: &str, reason: &str, now_ms: i64) -> Result<()>;

    /// **Prise en charge atomique** d'un run par un worker : `Queued → Running`
    /// **seulement si** le run est encore `Queued` ; pose `started_at_ms` et le
    /// battement à `now_ms`, incrémente la tentative. Renvoie le checkpoint
    /// réclamé (point de reprise compris), ou `None` si le run est inconnu ou
    /// déjà pris/terminé. Entre N répliques recevant la même livraison, une
    /// seule obtient `Some` : c'est ce qui rend l'exécution *au plus une fois*.
    async fn try_claim(&self, run_id: &str, now_ms: i64) -> Result<Option<RunCheckpoint>>;

    /// `Running → Failed` (raison + `finished_at_ms`) **seulement si** le run
    /// est encore `Running` ; `false` sinon (inconnu, ou terminé entre-temps).
    /// Contrairement à [`mark_failed`](Self::mark_failed), n'écrase jamais un
    /// run qui vient de réussir : sûr pour le balayage des orphelins.
    async fn fail_if_running(&self, run_id: &str, reason: &str, now_ms: i64) -> Result<bool>;

    /// Enregistre la progression de la tentative `attempt` (et son battement)
    /// **seulement si** le run est `Running` sous cette tentative ; `false`
    /// sinon (repris ailleurs, remis en file, clos, effacé) : le worker doit
    /// alors s'arrêter.
    async fn record_progress(
        &self,
        run_id: &str,
        attempt: i32,
        progress: &RunProgress,
        now_ms: i64,
    ) -> Result<bool>;

    /// Battement de la tentative `attempt` ; `false` dans les mêmes cas que
    /// [`record_progress`](Self::record_progress).
    async fn heartbeat(&self, run_id: &str, attempt: i32, now_ms: i64) -> Result<bool>;

    /// Runs `Running` dont le dernier battement est strictement antérieur à
    /// `before_ms` (worker tué ou coupé). Les runs sans battement (pris par un
    /// worker antérieur à #239) n'en font pas partie.
    async fn list_stale(&self, before_ms: i64) -> Result<Vec<RunCheckpoint>>;

    /// **Remise en file** de la tentative `attempt` : `Running → Queued`
    /// (point de reprise et nœud en cours conservés, battement effacé) et
    /// message de file repassé « à publier » sous une nouvelle génération,
    /// renvoyé pour être republié. `None` si le run n'est plus `Running` sous
    /// cette tentative, ou s'il n'a pas de message (run antérieur à l'outbox,
    /// qui ne peut pas être republié).
    async fn requeue(
        &self,
        run_id: &str,
        attempt: i32,
        now_ms: i64,
    ) -> Result<Option<OutboxMessage>>;

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

/// Message de l'outbox en mémoire et son état de publication.
#[derive(Debug, Clone)]
struct OutboxRow {
    message: OutboxMessage,
    created_at_ms: i64,
    published: bool,
}

#[derive(Debug, Default)]
struct Inner {
    runs: HashMap<String, RunCheckpoint>,
    outbox: HashMap<String, OutboxRow>,
}

impl Inner {
    /// Run `Running` sous la tentative `attempt`, s'il existe.
    fn running(&mut self, run_id: &str, attempt: i32) -> Option<&mut RunCheckpoint> {
        self.runs
            .get_mut(run_id)
            .filter(|c| c.status == RunStatus::Running && c.attempt == attempt)
    }
}

/// Impl de référence, in-process, thread-safe (un `Mutex` sur les runs et
/// leurs messages, pour que l'outbox reste atomique comme en base).
#[derive(Debug, Default)]
pub struct InMemoryRunStateStore {
    inner: Mutex<Inner>,
}

impl InMemoryRunStateStore {
    /// Crée un store vide.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| Error::Invalid("run_state lock empoisonné".into()))
    }

    /// Supprime tous les runs de `owner` (suppression de compte, #229) et
    /// leurs messages, et renvoie leur nombre. Les runs sans propriétaire ne
    /// sont jamais touchés.
    pub fn remove_owner(&self, owner: &str) -> Result<usize> {
        let mut inner = self.lock()?;
        let before = inner.runs.len();
        inner
            .runs
            .retain(|_, c| c.owner_id.as_deref() != Some(owner));
        let Inner { runs, outbox } = &mut *inner;
        outbox.retain(|id, _| runs.contains_key(id));
        Ok(before - runs.len())
    }
}

#[async_trait]
impl RunStateStore for InMemoryRunStateStore {
    async fn checkpoint(&self, snapshot: &RunCheckpoint) -> Result<()> {
        let mut inner = self.lock()?;
        // Propriétaire immuable (même règle que l'impl PostgreSQL) : un
        // checkpoint ultérieur ne peut ni l'effacer ni le changer.
        let mut snap = snapshot.clone();
        if let Some(existing) = inner.runs.get(&snapshot.run_id) {
            if existing.owner_id.is_some() {
                snap.owner_id = existing.owner_id.clone();
            }
        }
        inner.runs.insert(snapshot.run_id.clone(), snap);
        Ok(())
    }

    async fn enqueue(&self, snapshot: &RunCheckpoint, message: &OutboxMessage) -> Result<()> {
        let mut inner = self.lock()?;
        if inner.runs.contains_key(&snapshot.run_id) {
            return Err(Error::Invalid(format!(
                "enqueue : run {} déjà existant",
                snapshot.run_id
            )));
        }
        inner.runs.insert(snapshot.run_id.clone(), snapshot.clone());
        inner.outbox.insert(
            snapshot.run_id.clone(),
            OutboxRow {
                message: message.clone(),
                created_at_ms: snapshot.created_at_ms,
                published: false,
            },
        );
        Ok(())
    }

    async fn pending_outbox(
        &self,
        created_before_ms: i64,
        limit: usize,
    ) -> Result<Vec<OutboxMessage>> {
        let inner = self.lock()?;
        let mut pending: Vec<&OutboxRow> = inner
            .outbox
            .values()
            .filter(|r| !r.published && r.created_at_ms < created_before_ms)
            .collect();
        pending.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.message.run_id.cmp(&b.message.run_id))
        });
        Ok(pending
            .into_iter()
            .take(limit)
            .map(|r| r.message.clone())
            .collect())
    }

    async fn mark_published(&self, run_id: &str, generation: i32, _now_ms: i64) -> Result<()> {
        let mut inner = self.lock()?;
        if let Some(row) = inner.outbox.get_mut(run_id) {
            if row.message.generation == generation {
                row.published = true;
            }
        }
        Ok(())
    }

    async fn finish(&self, snapshot: &RunCheckpoint) -> Result<bool> {
        let mut inner = self.lock()?;
        let Some(existing) = inner.runs.get_mut(&snapshot.run_id) else {
            return Ok(false);
        };
        if existing.attempt != snapshot.attempt {
            return Ok(false);
        }
        let owner = existing.owner_id.clone();
        *existing = snapshot.clone();
        if owner.is_some() {
            existing.owner_id = owner;
        }
        inner.outbox.remove(&snapshot.run_id);
        Ok(true)
    }

    async fn load(&self, run_id: &str) -> Result<Option<RunCheckpoint>> {
        Ok(self.lock()?.runs.get(run_id).cloned())
    }

    async fn list_orphans(&self, older_than_ms: i64) -> Result<Vec<RunCheckpoint>> {
        let inner = self.lock()?;
        let mut orphans: Vec<RunCheckpoint> = inner
            .runs
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
        let mut inner = self.lock()?;
        let snap = inner
            .runs
            .get_mut(run_id)
            .ok_or_else(|| Error::Invalid(format!("mark_failed : run {run_id} inconnu")))?;
        if matches!(snap.status, RunStatus::Failed) {
            return Ok(()); // idempotent
        }
        snap.status = RunStatus::Failed;
        snap.error = Some(reason.to_string());
        snap.finished_at_ms = Some(now_ms);
        inner.outbox.remove(run_id);
        Ok(())
    }

    async fn try_claim(&self, run_id: &str, now_ms: i64) -> Result<Option<RunCheckpoint>> {
        let mut inner = self.lock()?;
        match inner.runs.get_mut(run_id) {
            Some(snap) if snap.status == RunStatus::Queued => {
                snap.status = RunStatus::Running;
                snap.started_at_ms = Some(now_ms);
                snap.heartbeat_at_ms = Some(now_ms);
                snap.attempt += 1;
                Ok(Some(snap.clone()))
            }
            _ => Ok(None),
        }
    }

    async fn fail_if_running(&self, run_id: &str, reason: &str, now_ms: i64) -> Result<bool> {
        let mut inner = self.lock()?;
        match inner.runs.get_mut(run_id) {
            Some(snap) if snap.status == RunStatus::Running => {
                snap.status = RunStatus::Failed;
                snap.error = Some(reason.to_string());
                snap.finished_at_ms = Some(now_ms);
                inner.outbox.remove(run_id);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn record_progress(
        &self,
        run_id: &str,
        attempt: i32,
        progress: &RunProgress,
        now_ms: i64,
    ) -> Result<bool> {
        let mut inner = self.lock()?;
        let Some(snap) = inner.running(run_id, attempt) else {
            return Ok(false);
        };
        match progress {
            RunProgress::Starting { node } => snap.next_node = Some(node.clone()),
            RunProgress::Done { resume_state } => {
                snap.next_node = None;
                snap.resume_state = Some(resume_state.clone());
            }
        }
        snap.heartbeat_at_ms = Some(now_ms);
        Ok(true)
    }

    async fn heartbeat(&self, run_id: &str, attempt: i32, now_ms: i64) -> Result<bool> {
        let mut inner = self.lock()?;
        let Some(snap) = inner.running(run_id, attempt) else {
            return Ok(false);
        };
        snap.heartbeat_at_ms = Some(now_ms);
        Ok(true)
    }

    async fn list_stale(&self, before_ms: i64) -> Result<Vec<RunCheckpoint>> {
        let inner = self.lock()?;
        let mut stale: Vec<RunCheckpoint> = inner
            .runs
            .values()
            .filter(|c| {
                c.status == RunStatus::Running && c.heartbeat_at_ms.is_some_and(|t| t < before_ms)
            })
            .cloned()
            .collect();
        stale.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        Ok(stale)
    }

    async fn requeue(
        &self,
        run_id: &str,
        attempt: i32,
        now_ms: i64,
    ) -> Result<Option<OutboxMessage>> {
        let mut inner = self.lock()?;
        if inner.running(run_id, attempt).is_none() {
            return Ok(None);
        }
        let Some(row) = inner.outbox.get_mut(run_id) else {
            return Ok(None);
        };
        row.message.generation += 1;
        row.message.message.headers.insert(
            "idempotency-key".into(),
            outbox_idempotency_key(run_id, row.message.generation),
        );
        row.created_at_ms = now_ms;
        row.published = false;
        let message = row.message.clone();
        let snap = inner.runs.get_mut(run_id).expect("run vérifié");
        snap.status = RunStatus::Queued;
        snap.heartbeat_at_ms = None;
        Ok(Some(message))
    }

    async fn list_by_workflow(
        &self,
        workflow_id: &str,
        owner: Option<&str>,
        paging: Pagination,
    ) -> Result<RunListPage> {
        let inner = self.lock()?;
        let mut all: Vec<RunCheckpoint> = inner
            .runs
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

    // ───── Claim atomique / échec conditionnel (#174) ─────

    #[tokio::test]
    async fn try_claim_moves_queued_to_running_once() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&snapshot("r1", RunStatus::Queued, None))
            .await
            .unwrap();

        let claimed = store.try_claim("r1", 5_000).await.unwrap().unwrap();
        assert_eq!(claimed.status, RunStatus::Running);
        assert_eq!(claimed.started_at_ms, Some(5_000));
        assert_eq!(store.load("r1").await.unwrap().unwrap(), claimed);

        // Redélivrance : déjà pris ⇒ None, état inchangé.
        assert!(store.try_claim("r1", 6_000).await.unwrap().is_none());
        assert_eq!(
            store.load("r1").await.unwrap().unwrap().started_at_ms,
            Some(5_000)
        );
    }

    #[tokio::test]
    async fn try_claim_refuses_finished_and_unknown_runs() {
        let store = InMemoryRunStateStore::new();
        for (id, status) in [("ok", RunStatus::Succeeded), ("ko", RunStatus::Failed)] {
            store
                .checkpoint(&snapshot(id, status, Some(1)))
                .await
                .unwrap();
            assert!(store.try_claim(id, 2).await.unwrap().is_none(), "{id}");
            assert_eq!(store.load(id).await.unwrap().unwrap().status, status);
        }
        assert!(store.try_claim("ghost", 2).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_claims_have_a_single_winner() {
        let store = std::sync::Arc::new(InMemoryRunStateStore::new());
        store
            .checkpoint(&snapshot("r1", RunStatus::Queued, None))
            .await
            .unwrap();
        let mut tasks = Vec::new();
        for i in 0..16 {
            let s = std::sync::Arc::clone(&store);
            tasks.push(tokio::spawn(
                async move { s.try_claim("r1", i).await.unwrap() },
            ));
        }
        let mut winners = 0;
        for t in tasks {
            winners += usize::from(t.await.unwrap().is_some());
        }
        assert_eq!(winners, 1);
    }

    #[tokio::test]
    async fn fail_if_running_never_overwrites_a_finished_run() {
        let store = InMemoryRunStateStore::new();
        store
            .checkpoint(&snapshot("run", RunStatus::Running, Some(1)))
            .await
            .unwrap();
        store
            .checkpoint(&snapshot("done", RunStatus::Succeeded, Some(1)))
            .await
            .unwrap();

        assert!(store.fail_if_running("run", "orphelin", 9).await.unwrap());
        let failed = store.load("run").await.unwrap().unwrap();
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("orphelin"));
        assert_eq!(failed.finished_at_ms, Some(9));
        // Déjà échoué, terminé avec succès ou inconnu : rien ne change.
        assert!(!store.fail_if_running("run", "encore", 10).await.unwrap());
        assert!(!store.fail_if_running("done", "orphelin", 9).await.unwrap());
        assert_eq!(
            store.load("done").await.unwrap().unwrap().status,
            RunStatus::Succeeded
        );
        assert!(!store.fail_if_running("ghost", "x", 9).await.unwrap());
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

    #[tokio::test]
    async fn finish_writes_only_a_run_that_still_exists() {
        let store = InMemoryRunStateStore::new();
        let queued = run_with_created("r", "wf", 1).with_owner(Some("alice".into()));
        store.checkpoint(&queued).await.unwrap();
        let mut done = run_with_created("r", "wf", 1);
        done.status = RunStatus::Succeeded;
        done.outputs
            .insert("n".into(), serde_json::json!({ "ok": true }));
        assert!(store.finish(&done).await.unwrap());
        let loaded = store.load("r").await.unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Succeeded);
        assert_eq!(loaded.outputs.len(), 1);
        assert_eq!(
            loaded.owner_id.as_deref(),
            Some("alice"),
            "propriétaire gardé"
        );

        // Run effacé pendant l'exécution : rien n'est recréé.
        assert_eq!(store.remove_owner("alice").unwrap(), 1);
        assert!(!store.finish(&done).await.unwrap());
        assert_eq!(store.load("r").await.unwrap(), None);
    }

    #[tokio::test]
    async fn remove_owner_spares_other_owners_and_ownerless_runs() {
        let store = InMemoryRunStateStore::new();
        for (id, owner) in [
            ("a1", Some("alice")),
            ("a2", Some("alice")),
            ("b", Some("bob")),
            ("x", None),
        ] {
            store
                .checkpoint(&run_with_created(id, "wf", 1).with_owner(owner.map(String::from)))
                .await
                .unwrap();
        }
        assert_eq!(store.remove_owner("alice").unwrap(), 2);
        assert_eq!(store.remove_owner("alice").unwrap(), 0);
        assert!(store.load("b").await.unwrap().is_some());
        assert!(store.load("x").await.unwrap().is_some());
    }

    // ───── Outbox et reprise (#239) ─────

    fn outbox_for(run_id: &str) -> OutboxMessage {
        OutboxMessage {
            run_id: run_id.into(),
            queue: "runs".into(),
            generation: 0,
            message: Message::new(json!({ "run_id": run_id, "payload": { "k": 1 } }))
                .with_header("idempotency-key", outbox_idempotency_key(run_id, 0)),
        }
    }

    /// Run en file avec son message, puis pris par un worker.
    async fn claimed(store: &InMemoryRunStateStore, run_id: &str) -> RunCheckpoint {
        store
            .enqueue(&run_with_created(run_id, "wf", 10), &outbox_for(run_id))
            .await
            .unwrap();
        store.mark_published(run_id, 0, 11).await.unwrap();
        store.try_claim(run_id, 100).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn enqueue_writes_the_run_and_a_pending_message() {
        let store = InMemoryRunStateStore::new();
        store
            .enqueue(&run_with_created("r", "wf", 10), &outbox_for("r"))
            .await
            .unwrap();
        assert_eq!(
            store.load("r").await.unwrap().unwrap().status,
            RunStatus::Queued
        );
        // Seuil strict sur la date de création : le relais laisse au
        // dispatcher le temps de publier lui-même.
        assert!(store.pending_outbox(10, 10).await.unwrap().is_empty());
        assert_eq!(
            store.pending_outbox(11, 10).await.unwrap(),
            vec![outbox_for("r")]
        );
        // Un id déjà pris est refusé, sans toucher au run existant.
        assert!(store
            .enqueue(&run_with_created("r", "wf", 99), &outbox_for("r"))
            .await
            .is_err());
        store.mark_published("r", 0, 12).await.unwrap();
        assert!(store.pending_outbox(i64::MAX, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_outbox_is_ordered_and_bounded() {
        let store = InMemoryRunStateStore::new();
        for (id, created) in [("c", 30), ("a", 10), ("b", 20)] {
            store
                .enqueue(&run_with_created(id, "wf", created), &outbox_for(id))
                .await
                .unwrap();
        }
        let ids: Vec<String> = store
            .pending_outbox(i64::MAX, 2)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.run_id)
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn claim_starts_a_new_attempt_with_a_heartbeat() {
        let store = InMemoryRunStateStore::new();
        let first = claimed(&store, "r").await;
        assert_eq!(first.attempt, 1);
        assert_eq!(first.heartbeat_at_ms, Some(100));
        store.requeue("r", 1, 200).await.unwrap().unwrap();
        let second = store.try_claim("r", 300).await.unwrap().unwrap();
        assert_eq!(second.attempt, 2);
        assert_eq!(second.started_at_ms, Some(300));
    }

    #[tokio::test]
    async fn progress_and_heartbeat_are_fenced_by_the_attempt() {
        let store = InMemoryRunStateStore::new();
        claimed(&store, "r").await;
        let starting = RunProgress::Starting { node: "a".into() };
        assert!(store.record_progress("r", 1, &starting, 150).await.unwrap());
        let cp = store.load("r").await.unwrap().unwrap();
        assert_eq!(cp.next_node.as_deref(), Some("a"));
        assert_eq!(cp.heartbeat_at_ms, Some(150));

        let done = RunProgress::Done {
            resume_state: json!({ "done": ["a"] }),
        };
        assert!(store.record_progress("r", 1, &done, 160).await.unwrap());
        let cp = store.load("r").await.unwrap().unwrap();
        assert_eq!(cp.next_node, None);
        assert_eq!(cp.resume_state, Some(json!({ "done": ["a"] })));
        assert!(store.heartbeat("r", 1, 170).await.unwrap());

        // Autre tentative, run remis en file, inconnu : rien n'est écrit.
        assert!(!store.record_progress("r", 2, &starting, 180).await.unwrap());
        assert!(!store.heartbeat("r", 2, 180).await.unwrap());
        store.requeue("r", 1, 190).await.unwrap().unwrap();
        assert!(!store.record_progress("r", 1, &starting, 200).await.unwrap());
        assert!(!store.heartbeat("r", 1, 200).await.unwrap());
        assert!(!store.heartbeat("ghost", 1, 200).await.unwrap());
        assert_eq!(
            store.load("r").await.unwrap().unwrap().heartbeat_at_ms,
            None
        );
    }

    #[tokio::test]
    async fn list_stale_returns_running_runs_whose_heartbeat_stopped() {
        let store = InMemoryRunStateStore::new();
        claimed(&store, "old").await; // battement à 100
        claimed(&store, "fresh").await;
        assert!(store.heartbeat("fresh", 1, 500).await.unwrap());
        // Pris par un worker antérieur à #239 : pas de battement, ignoré.
        store
            .checkpoint(&snapshot("legacy", RunStatus::Running, Some(1)))
            .await
            .unwrap();
        let stale: Vec<String> = store
            .list_stale(400)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.run_id)
            .collect();
        assert_eq!(stale, vec!["old"]);
        assert!(store.list_stale(100).await.unwrap().is_empty(), "strict");
    }

    #[tokio::test]
    async fn requeue_republishes_under_a_new_generation_and_keeps_the_cursor() {
        let store = InMemoryRunStateStore::new();
        claimed(&store, "r").await;
        let done = RunProgress::Done {
            resume_state: json!({ "cursor": 1 }),
        };
        store.record_progress("r", 1, &done, 150).await.unwrap();
        store
            .record_progress("r", 1, &RunProgress::Starting { node: "b".into() }, 160)
            .await
            .unwrap();

        let msg = store.requeue("r", 1, 200).await.unwrap().unwrap();
        assert_eq!(msg.generation, 1);
        assert_eq!(msg.message.body["payload"], json!({ "k": 1 }));
        assert_eq!(
            msg.message
                .headers
                .get("idempotency-key")
                .map(String::as_str),
            Some("r#1")
        );
        let cp = store.load("r").await.unwrap().unwrap();
        assert_eq!(cp.status, RunStatus::Queued);
        assert_eq!(cp.resume_state, Some(json!({ "cursor": 1 })));
        assert_eq!(cp.next_node.as_deref(), Some("b"), "nœud en cours gardé");
        // À publier de nouveau, à partir de la date de remise en file.
        assert!(store.pending_outbox(200, 10).await.unwrap().is_empty());
        assert_eq!(store.pending_outbox(201, 10).await.unwrap(), vec![msg]);
        // Un relais en retard sur la génération 0 ne la masque pas.
        store.mark_published("r", 0, 210).await.unwrap();
        assert_eq!(store.pending_outbox(i64::MAX, 10).await.unwrap().len(), 1);
        store.mark_published("r", 1, 220).await.unwrap();
        assert!(store.pending_outbox(i64::MAX, 10).await.unwrap().is_empty());

        // Plus `running` : pas de seconde remise en file.
        assert!(store.requeue("r", 1, 300).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn requeue_refuses_another_attempt_and_runs_without_message() {
        let store = InMemoryRunStateStore::new();
        claimed(&store, "r").await;
        assert!(store.requeue("r", 2, 200).await.unwrap().is_none());
        assert_eq!(
            store.load("r").await.unwrap().unwrap().status,
            RunStatus::Running
        );
        // Run antérieur à l'outbox : rien à republier, il reste `running`.
        store
            .checkpoint(&snapshot("legacy", RunStatus::Queued, None))
            .await
            .unwrap();
        store.try_claim("legacy", 1).await.unwrap().unwrap();
        assert!(store.requeue("legacy", 1, 2).await.unwrap().is_none());
        assert_eq!(
            store.load("legacy").await.unwrap().unwrap().status,
            RunStatus::Running
        );
    }

    #[tokio::test]
    async fn finish_is_fenced_by_the_attempt_and_drops_the_message() {
        let store = InMemoryRunStateStore::new();
        let mut first = claimed(&store, "r").await;
        store.requeue("r", 1, 200).await.unwrap().unwrap();
        let second = store.try_claim("r", 300).await.unwrap().unwrap();

        // Le worker de la première tentative ne peut plus rien écrire.
        first.status = RunStatus::Succeeded;
        assert!(!store.finish(&first).await.unwrap());
        assert_eq!(
            store.load("r").await.unwrap().unwrap().status,
            RunStatus::Running
        );

        let mut done = second;
        done.status = RunStatus::Succeeded;
        assert!(store.finish(&done).await.unwrap());
        // Run terminé : son message (et le payload qu'il porte) disparaît.
        assert!(store.requeue("r", 2, 400).await.unwrap().is_none());
        assert!(store.lock().unwrap().outbox.is_empty());
    }

    #[tokio::test]
    async fn failing_or_erasing_a_run_drops_its_message() {
        let store = InMemoryRunStateStore::new();
        claimed(&store, "f").await;
        assert!(store.fail_if_running("f", "x", 200).await.unwrap());
        claimed(&store, "m").await;
        store.mark_failed("m", "x", 200).await.unwrap();
        store
            .enqueue(
                &run_with_created("e", "wf", 1).with_owner(Some("alice".into())),
                &outbox_for("e"),
            )
            .await
            .unwrap();
        assert_eq!(store.remove_owner("alice").unwrap(), 1);
        assert!(store.lock().unwrap().outbox.is_empty());
    }

    #[test]
    fn idempotency_key_follows_the_generation() {
        assert_eq!(outbox_idempotency_key("run-1", 0), "run-1");
        assert_eq!(outbox_idempotency_key("run-1", 3), "run-1#3");
    }

    #[test]
    fn resume_state_is_never_serialized() {
        let mut cp = run_with_created("r", "wf", 1);
        cp.resume_state = Some(json!({ "secret": "payload" }));
        let json = serde_json::to_value(&cp).unwrap();
        assert!(json.get("resume_state").is_none());
        assert_eq!(json["attempt"], 0);
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

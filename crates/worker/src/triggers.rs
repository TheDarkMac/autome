//! Boucle de planification des déclencheurs `schedule` et `polling` (#227).
//!
//! Chaque worker fait tourner la boucle ; les échéances vivent dans un
//! [`TriggerScheduleStore`] partagé. À chaque passe ([`TriggerLoop::tick`]) :
//!
//! 1. lecture des échéances atteintes ou à calculer ;
//! 2. nœud nouveau ou modifié (échéance `None`) : **armé** — prochaine
//!    échéance calculée, sans run (un polling est armé pour tout de suite : son
//!    premier poll sert de référence) ;
//! 3. échéance atteinte : avancée par compare-and-set sur la révision. Seule la
//!    réplique qui réussit déclenche ; les autres passent. Le run est dispatché
//!    **après** l'avancée : un worker tué entre les deux perd ce déclenchement
//!    (au plus une fois, comme l'exécution des runs, #174) ;
//! 4. `schedule` en retard de plus de [`MISFIRE_GRACE_MS`] (workers arrêtés) :
//!    échéance sautée, pas de rafale de rattrapage ;
//! 5. `polling` : `GET` par le client HTTP filtré anti-SSRF dans une tâche à
//!    part (un endpoint lent ne retarde pas les autres échéances), nouveautés
//!    calculées par [`PollingSpec::step`], état enregistré par un second
//!    compare-and-set **avant** de dispatcher (une réplique qui aurait déjà
//!    repris le nœud les traite à sa place).
//!
//! Paramètres inutilisables (données antérieures à la validation de l'API) :
//! nœud marqué en erreur, ignoré jusqu'à sa prochaine modification.
//!
//! Payload du nœud trigger : `{ node_id, scheduled_at, scheduled_at_ms }` pour
//! un `schedule`, `{ node_id, item }` (un run par nouvel élément) pour un
//! `polling`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use autome_connectors::{rfc3339_utc, HttpClient, HttpRequest, PollingSpec, TriggerSpec};
use autome_core::{
    DispatchRequest, Error, Result, RunDispatcher, ScheduleSlot, SlotUpdate, TriggerScheduleStore,
};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::metrics::{TriggerKind, WorkerMetrics};

/// Retard au-delà duquel une échéance `schedule` est sautée (5 min).
pub const MISFIRE_GRACE_MS: i64 = 5 * 60_000;
/// Échéances traitées au plus par passe.
pub const BATCH_SIZE: usize = 100;
/// Période par défaut de la boucle (`AUTOME_SCHEDULER_TICK_SECS`).
pub const DEFAULT_TICK_SECS: u64 = 10;

/// Lit `AUTOME_SCHEDULER_TICK_SECS` : période de la boucle, `0` = boucle
/// désactivée (`None`). Valeur non entière ⇒ refus de démarrer.
pub fn tick_period_from_env() -> Result<Option<Duration>> {
    parse_tick_period(std::env::var("AUTOME_SCHEDULER_TICK_SECS").ok().as_deref())
}

fn parse_tick_period(raw: Option<&str>) -> Result<Option<Duration>> {
    let secs = match raw.map(str::trim) {
        None | Some("") => DEFAULT_TICK_SECS,
        Some(v) => v.parse::<u64>().map_err(|_| {
            Error::Invalid(format!(
                "AUTOME_SCHEDULER_TICK_SECS invalide : `{v}` (secondes, 0 = désactivé)"
            ))
        })?,
    };
    Ok((secs > 0).then(|| Duration::from_secs(secs)))
}

/// Bilan d'une passe.
#[derive(Debug, Default)]
pub struct TickOutcome {
    /// Échéances lues.
    pub processed: usize,
    /// Nœuds armés (première échéance calculée).
    pub armed: usize,
    /// Runs `schedule` dispatchés.
    pub fired: usize,
    /// Échéances `schedule` sautées (retard).
    pub missed: usize,
    /// Polls lancés : chacun renvoie le nombre de runs dispatchés.
    pub polls: Vec<JoinHandle<usize>>,
}

/// Boucle de planification. Clonable à bas coût (tout est partagé).
#[derive(Clone)]
pub struct TriggerLoop {
    store: Arc<dyn TriggerScheduleStore>,
    dispatcher: Arc<dyn RunDispatcher + Send + Sync>,
    http: Arc<dyn HttpClient + Send + Sync>,
    /// Runs déclenchés, échéances manquées, polls en échec (#235).
    metrics: Arc<WorkerMetrics>,
}

impl TriggerLoop {
    /// Boucle sur le store d'échéances, le dispatcher des runs et le client
    /// HTTP des polls (filtré anti-SSRF en production).
    pub fn new(
        store: Arc<dyn TriggerScheduleStore>,
        dispatcher: Arc<dyn RunDispatcher + Send + Sync>,
        http: Arc<dyn HttpClient + Send + Sync>,
    ) -> Self {
        Self {
            store,
            dispatcher,
            http,
            metrics: Arc::new(WorkerMetrics::new()),
        }
    }

    /// Builder : registre où compter les déclenchements (#235) ; par défaut
    /// un registre propre à la boucle.
    pub fn with_metrics(mut self, metrics: Arc<WorkerMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Une passe à l'instant `now_ms`. `Err` seulement si les échéances ne
    /// peuvent pas être lues ; les échecs par nœud sont journalisés.
    pub async fn tick(&self, now_ms: i64) -> Result<TickOutcome> {
        let slots = self.store.due(now_ms, BATCH_SIZE).await?;
        let mut outcome = TickOutcome {
            processed: slots.len(),
            ..TickOutcome::default()
        };
        for slot in slots {
            if let Err(e) = self.process(slot.clone(), now_ms, &mut outcome).await {
                tracing::error!(
                    owner_id = %slot.owner_id,
                    workflow_id = %slot.workflow_id,
                    node_id = %slot.node_id,
                    error = %e,
                    "planification : échéance non traitée"
                );
            }
        }
        Ok(outcome)
    }

    async fn process(&self, slot: ScheduleSlot, now_ms: i64, out: &mut TickOutcome) -> Result<()> {
        let spec = match TriggerSpec::parse(&slot.trigger_type, &slot.params) {
            Ok(Some(spec)) => spec,
            Ok(None) => return self.disable(&slot, "type de trigger non planifiable").await,
            Err(e) => return self.disable(&slot, &e).await,
        };

        let Some(due) = slot.next_fire_ms else {
            let first = match spec {
                TriggerSpec::Polling(_) => Some(now_ms),
                _ => spec.next_after(now_ms),
            };
            let Some(first) = first else {
                return self.disable(&slot, "aucune échéance à venir").await;
            };
            if self
                .store
                .advance(&slot, keep_state(&slot, Some(first)))
                .await?
            {
                out.armed += 1;
            }
            return Ok(());
        };

        let next = spec
            .next_after(due)
            .filter(|&t| t > now_ms)
            .or_else(|| spec.next_after(now_ms));
        let Some(next) = next else {
            return self.disable(&slot, "aucune échéance à venir").await;
        };
        if !self
            .store
            .advance(&slot, keep_state(&slot, Some(next)))
            .await?
        {
            // Une autre réplique a pris l'échéance, ou le nœud a changé.
            return Ok(());
        }

        if let TriggerSpec::Polling(polling) = spec {
            let claimed = ScheduleSlot {
                next_fire_ms: Some(next),
                revision: slot.revision + 1,
                ..slot
            };
            let this = self.clone();
            out.polls.push(tokio::spawn(async move {
                this.poll(claimed, polling, now_ms).await
            }));
            return Ok(());
        }

        if now_ms - due > MISFIRE_GRACE_MS {
            tracing::warn!(
                owner_id = %slot.owner_id,
                workflow_id = %slot.workflow_id,
                node_id = %slot.node_id,
                scheduled_at = %rfc3339_utc(due),
                "planification : échéance manquée (retard), sautée"
            );
            out.missed += 1;
            self.metrics.trigger_missed();
            return Ok(());
        }
        let payload = json!({
            "node_id": slot.node_id,
            "scheduled_at": rfc3339_utc(due),
            "scheduled_at_ms": due,
        });
        if self.dispatch(&slot, payload, now_ms).await {
            out.fired += 1;
        }
        Ok(())
    }

    /// Exécute un poll déjà réclamé ; renvoie le nombre de runs dispatchés.
    async fn poll(&self, claimed: ScheduleSlot, spec: PollingSpec, now_ms: i64) -> usize {
        let request = HttpRequest {
            method: "GET".into(),
            url: spec.url.clone(),
            headers: BTreeMap::from([("accept".to_string(), "application/json".to_string())]),
            body: None,
        };
        let http = Arc::clone(&self.http);
        let response = tokio::task::spawn_blocking(move || http.send(&request))
            .await
            .map_err(|e| format!("tâche du poll interrompue : {e}"))
            .and_then(|r| r)
            .and_then(|r| {
                if (200..300).contains(&r.status) {
                    Ok(r.body)
                } else {
                    Err(format!("statut HTTP {}", r.status))
                }
            })
            .and_then(|body| spec.items(&body));
        let items = match response {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!(
                    owner_id = %claimed.owner_id,
                    workflow_id = %claimed.workflow_id,
                    node_id = %claimed.node_id,
                    error = %e,
                    "polling : poll en échec, nouvel essai à la prochaine échéance"
                );
                self.metrics.trigger_poll_error();
                return 0;
            }
        };

        let (fresh, state) = spec.step(&claimed.state, &items);
        if state != claimed.state {
            let update = SlotUpdate {
                next_fire_ms: claimed.next_fire_ms,
                state,
                error: None,
            };
            match self.store.advance(&claimed, update).await {
                Ok(true) => {}
                // Nœud repris (autre réplique) ou modifié entre-temps.
                Ok(false) => return 0,
                Err(e) => {
                    tracing::error!(
                        workflow_id = %claimed.workflow_id,
                        node_id = %claimed.node_id,
                        error = %e,
                        "polling : état non enregistré, aucun run"
                    );
                    return 0;
                }
            }
        }

        let mut dispatched = 0;
        for item in fresh {
            let payload = json!({ "node_id": claimed.node_id, "item": item });
            if self.dispatch(&claimed, payload, now_ms).await {
                dispatched += 1;
            }
        }
        dispatched
    }

    async fn dispatch(&self, slot: &ScheduleSlot, payload: Value, now_ms: i64) -> bool {
        let request = DispatchRequest {
            workflow_id: slot.workflow_id.clone(),
            version: 1,
            trigger_type: slot.trigger_type.clone(),
            payload,
            now_ms,
            owner_id: Some(slot.owner_id.clone()),
        };
        let result = self.dispatcher.dispatch(request).await;
        self.metrics
            .trigger_run(TriggerKind::of(&slot.trigger_type), result.is_ok());
        match result {
            Ok(outcome) => {
                tracing::info!(
                    run_id = %outcome.run_id,
                    workflow_id = %slot.workflow_id,
                    node_id = %slot.node_id,
                    trigger_type = %slot.trigger_type,
                    "planification : run déclenché"
                );
                true
            }
            Err(e) => {
                tracing::error!(
                    workflow_id = %slot.workflow_id,
                    node_id = %slot.node_id,
                    error = %e,
                    "planification : dispatch du run impossible, déclenchement perdu"
                );
                false
            }
        }
    }

    async fn disable(&self, slot: &ScheduleSlot, reason: &str) -> Result<()> {
        tracing::warn!(
            owner_id = %slot.owner_id,
            workflow_id = %slot.workflow_id,
            node_id = %slot.node_id,
            reason,
            "planification : nœud ignoré jusqu'à sa prochaine modification"
        );
        let update = SlotUpdate {
            next_fire_ms: None,
            state: slot.state.clone(),
            error: Some(reason.to_string()),
        };
        self.store.advance(slot, update).await.map(|_| ())
    }

    /// Lance la boucle : une passe toutes les `period` (immédiatement à
    /// nouveau si la passe était pleine). `clock` donne l'heure (ms Unix).
    pub fn spawn(self, period: Duration, clock: fn() -> i64) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                loop {
                    match self.tick(clock()).await {
                        Ok(outcome) if outcome.processed == BATCH_SIZE => continue,
                        Ok(_) => break,
                        Err(e) => {
                            self.metrics.trigger_tick_error();
                            tracing::error!(error = %e, "planification : échéances illisibles");
                            break;
                        }
                    }
                }
            }
        })
    }
}

fn keep_state(slot: &ScheduleSlot, next_fire_ms: Option<i64>) -> SlotUpdate {
    SlotUpdate {
        next_fire_ms,
        state: slot.state.clone(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use autome_connectors::HttpResponse;
    use autome_core::{DispatchOutcome, InMemoryTriggerScheduleStore, ScheduledTrigger};

    /// Dispatcher qui enregistre les demandes.
    #[derive(Default)]
    struct Recorder {
        requests: Mutex<Vec<DispatchRequest>>,
        fail: bool,
    }

    #[async_trait]
    impl RunDispatcher for Recorder {
        async fn dispatch(&self, req: DispatchRequest) -> Result<DispatchOutcome> {
            if self.fail {
                return Err(Error::Invalid("broker indisponible".into()));
            }
            let mut requests = self.requests.lock().unwrap();
            requests.push(req);
            Ok(DispatchOutcome {
                run_id: format!("run-{}", requests.len()),
            })
        }
    }

    impl Recorder {
        fn payloads(&self) -> Vec<Value> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.payload.clone())
                .collect()
        }
    }

    /// Client HTTP qui renvoie la réponse courante et compte les appels.
    struct FakeHttp {
        response: Mutex<std::result::Result<HttpResponse, String>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeHttp {
        fn new(body: Value) -> Arc<Self> {
            Arc::new(Self {
                response: Mutex::new(Ok(HttpResponse { status: 200, body })),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn set(&self, response: std::result::Result<HttpResponse, String>) {
            *self.response.lock().unwrap() = response;
        }
    }

    impl HttpClient for FakeHttp {
        fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{} {}", req.method, req.url));
            self.response.lock().unwrap().clone()
        }
    }

    const MIN: i64 = 60_000;
    /// 2026-10-17T10:00:00Z.
    const T0: i64 = 1_792_231_200_000;

    struct Harness {
        store: Arc<InMemoryTriggerScheduleStore>,
        runs: Arc<Recorder>,
        http: Arc<FakeHttp>,
        lp: TriggerLoop,
    }

    fn harness(body: Value) -> Harness {
        let store = Arc::new(InMemoryTriggerScheduleStore::new());
        let runs = Arc::new(Recorder::default());
        let http = FakeHttp::new(body);
        let lp = TriggerLoop::new(store.clone(), runs.clone(), http.clone());
        Harness {
            store,
            runs,
            http,
            lp,
        }
    }

    async fn sync(h: &Harness, trigger_type: &str, params: Value) {
        h.store
            .sync(
                "alice",
                "wf",
                &[ScheduledTrigger {
                    node_id: "t".into(),
                    trigger_type: trigger_type.into(),
                    params,
                }],
            )
            .await
            .unwrap();
    }

    async fn next_fire(h: &Harness) -> Option<i64> {
        h.store.due(i64::MAX, 10).await.unwrap()[0].next_fire_ms
    }

    async fn polls(outcome: TickOutcome) -> usize {
        let mut total = 0;
        for p in outcome.polls {
            total += p.await.unwrap();
        }
        total
    }

    #[test]
    fn tick_period_defaults_and_can_be_disabled() {
        assert_eq!(
            parse_tick_period(None).unwrap(),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            parse_tick_period(Some(" 3 ")).unwrap(),
            Some(Duration::from_secs(3))
        );
        assert_eq!(parse_tick_period(Some("0")).unwrap(), None);
        assert!(parse_tick_period(Some("dix")).is_err());
    }

    #[tokio::test]
    async fn cron_is_armed_then_fires_once_per_slot() {
        let h = harness(json!(null));
        sync(&h, "schedule", json!({ "cron": "*/5 * * * *" })).await;

        // Armement : aucune exécution, prochaine échéance 10:05.
        let out = h.lp.tick(T0 + 1_000).await.unwrap();
        assert_eq!((out.armed, out.fired), (1, 0));
        assert_eq!(next_fire(&h).await, Some(T0 + 5 * MIN));

        // Avant l'échéance : rien.
        assert_eq!(h.lp.tick(T0 + 4 * MIN).await.unwrap().processed, 0);

        // À l'échéance (passe 3 s plus tard) : un run, échéance suivante 10:10.
        let out = h.lp.tick(T0 + 5 * MIN + 3_000).await.unwrap();
        assert_eq!(out.fired, 1);
        assert_eq!(next_fire(&h).await, Some(T0 + 10 * MIN));
        let req = h.runs.requests.lock().unwrap()[0].clone();
        assert_eq!(req.workflow_id, "wf");
        assert_eq!(req.owner_id.as_deref(), Some("alice"));
        assert_eq!(req.trigger_type, "schedule");
        assert_eq!(
            req.payload,
            json!({
                "node_id": "t",
                "scheduled_at": "2026-10-17T10:05:00Z",
                "scheduled_at_ms": T0 + 5 * MIN,
            })
        );
        // Même instant, nouvelle passe : déjà avancée.
        assert_eq!(h.lp.tick(T0 + 5 * MIN + 4_000).await.unwrap().fired, 0);
    }

    #[tokio::test]
    async fn two_replicas_fire_a_slot_exactly_once() {
        let h = harness(json!(null));
        sync(&h, "schedule", json!({ "interval_seconds": 60 })).await;
        h.lp.tick(T0).await.unwrap();
        // Deux répliques partageant le store et le dispatcher, en parallèle.
        let other = TriggerLoop::new(h.store.clone(), h.runs.clone(), h.http.clone());
        for round in 1..=20 {
            let now = T0 + round * MIN;
            let (a, b) = tokio::join!(h.lp.tick(now), other.tick(now));
            assert_eq!(a.unwrap().fired + b.unwrap().fired, 1, "round {round}");
        }
        assert_eq!(h.runs.requests.lock().unwrap().len(), 20);
    }

    #[tokio::test]
    async fn interval_after_a_short_delay_keeps_its_cadence() {
        let h = harness(json!(null));
        sync(&h, "schedule", json!({ "interval_seconds": 30 })).await;
        h.lp.tick(T0).await.unwrap();
        assert_eq!(next_fire(&h).await, Some(T0 + 30_000));
        h.lp.tick(T0 + 32_000).await.unwrap();
        assert_eq!(next_fire(&h).await, Some(T0 + 60_000));
        // Passe très en retard (2 intervalles) : un run, cadence reprise depuis maintenant.
        let out = h.lp.tick(T0 + 125_000).await.unwrap();
        assert_eq!(out.fired, 1);
        assert_eq!(next_fire(&h).await, Some(T0 + 155_000));
    }

    #[tokio::test]
    async fn slots_missed_while_workers_were_down_are_skipped() {
        let h = harness(json!(null));
        sync(&h, "schedule", json!({ "cron": "0 * * * *" })).await;
        h.lp.tick(T0 - MIN).await.unwrap();
        assert_eq!(next_fire(&h).await, Some(T0));
        // Workers arrêtés 3 h : pas de rafale, échéance suivante après maintenant.
        let out = h.lp.tick(T0 + 3 * 60 * MIN + 10 * MIN).await.unwrap();
        assert_eq!((out.fired, out.missed), (0, 1));
        assert_eq!(next_fire(&h).await, Some(T0 + 4 * 60 * MIN));
        // Dans la tolérance : déclenché.
        let out = h.lp.tick(T0 + 4 * 60 * MIN + 4 * MIN).await.unwrap();
        assert_eq!(out.fired, 1);
    }

    #[tokio::test]
    async fn invalid_parameters_disable_the_node() {
        let h = harness(json!(null));
        sync(&h, "schedule", json!({ "cron": "nope" })).await;
        let out = h.lp.tick(T0).await.unwrap();
        assert_eq!((out.armed, out.fired), (0, 0));
        assert!(h.store.due(i64::MAX, 10).await.unwrap().is_empty());
        // Corrigé par l'utilisateur : repris.
        sync(&h, "schedule", json!({ "cron": "* * * * *" })).await;
        assert_eq!(h.lp.tick(T0).await.unwrap().armed, 1);
    }

    #[tokio::test]
    async fn dispatch_failure_loses_only_that_slot() {
        let store = Arc::new(InMemoryTriggerScheduleStore::new());
        let failing = Arc::new(Recorder {
            fail: true,
            ..Recorder::default()
        });
        let lp = TriggerLoop::new(store.clone(), failing, FakeHttp::new(json!(null)));
        store
            .sync(
                "alice",
                "wf",
                &[ScheduledTrigger {
                    node_id: "t".into(),
                    trigger_type: "schedule".into(),
                    params: json!({ "interval_seconds": 60 }),
                }],
            )
            .await
            .unwrap();
        lp.tick(T0).await.unwrap();
        let out = lp.tick(T0 + MIN).await.unwrap();
        assert_eq!(out.fired, 0);
        // Échéance avancée malgré tout : pas de reprise en boucle.
        assert_eq!(
            store.due(i64::MAX, 10).await.unwrap()[0].next_fire_ms,
            Some(T0 + 2 * MIN)
        );
    }

    #[tokio::test]
    async fn firings_misses_failed_dispatches_and_poll_errors_are_counted() {
        use crate::metrics::Gauges;
        let metrics = Arc::new(WorkerMetrics::new());

        // Intervalle : un run déclenché, puis une échéance trop en retard.
        let h = harness(json!(null));
        let lp = h.lp.clone().with_metrics(Arc::clone(&metrics));
        sync(&h, "schedule", json!({ "interval_seconds": 60 })).await;
        lp.tick(T0).await.unwrap();
        assert_eq!(lp.tick(T0 + MIN).await.unwrap().fired, 1);
        assert_eq!(lp.tick(T0 + 12 * MIN).await.unwrap().missed, 1);

        // Polling : poll en échec.
        let p = harness(json!(null));
        let lp = p.lp.clone().with_metrics(Arc::clone(&metrics));
        sync(
            &p,
            "polling",
            json!({ "url": "https://api.example/items", "interval_seconds": 30 }),
        )
        .await;
        lp.tick(T0).await.unwrap();
        p.http.set(Err("connexion refusée".into()));
        assert_eq!(polls(lp.tick(T0).await.unwrap()).await, 0);

        // Dispatch impossible : run perdu.
        let store = Arc::new(InMemoryTriggerScheduleStore::new());
        let failing = Arc::new(Recorder {
            fail: true,
            ..Recorder::default()
        });
        let lp = TriggerLoop::new(store.clone(), failing, FakeHttp::new(json!(null)))
            .with_metrics(Arc::clone(&metrics));
        store
            .sync(
                "alice",
                "wf",
                &[ScheduledTrigger {
                    node_id: "t".into(),
                    trigger_type: "schedule".into(),
                    params: json!({ "interval_seconds": 60 }),
                }],
            )
            .await
            .unwrap();
        lp.tick(T0).await.unwrap();
        lp.tick(T0 + MIN).await.unwrap();

        let out = metrics.render(Gauges::default());
        for line in [
            "autome_worker_trigger_runs_total{kind=\"schedule\",result=\"dispatched\"} 1",
            "autome_worker_trigger_runs_total{kind=\"schedule\",result=\"failed\"} 1",
            "autome_worker_trigger_runs_total{kind=\"polling\",result=\"dispatched\"} 0",
            "autome_worker_trigger_missed_total 1",
            "autome_worker_trigger_poll_errors_total 1",
        ] {
            assert!(out.contains(&format!("{line}\n")), "{line} absent :\n{out}");
        }
    }

    #[tokio::test]
    async fn polling_baselines_then_fires_one_run_per_new_item() {
        let h = harness(json!({ "data": [{ "id": 1 }, { "id": 2 }] }));
        sync(
            &h,
            "polling",
            json!({ "url": "https://api.example/items", "interval_seconds": 30, "items_path": "data" }),
        )
        .await;

        // Armé pour tout de suite, puis premier poll = référence.
        assert_eq!(h.lp.tick(T0).await.unwrap().armed, 1);
        let out = h.lp.tick(T0).await.unwrap();
        assert_eq!(polls(out).await, 0);
        assert_eq!(
            h.http.calls.lock().unwrap().as_slice(),
            ["GET https://api.example/items"]
        );
        assert_eq!(next_fire(&h).await, Some(T0 + 30_000));

        // Nouveaux éléments au poll suivant.
        h.http.set(Ok(HttpResponse {
            status: 200,
            body: json!({ "data": [{ "id": 3 }, { "id": 1 }, { "id": 2 }, { "id": 4 }] }),
        }));
        assert_eq!(polls(h.lp.tick(T0 + 30_000).await.unwrap()).await, 2);
        assert_eq!(
            h.runs.payloads(),
            vec![
                json!({ "node_id": "t", "item": { "id": 3 } }),
                json!({ "node_id": "t", "item": { "id": 4 } }),
            ]
        );
        assert_eq!(h.runs.requests.lock().unwrap()[0].trigger_type, "polling");
        // Rien de neuf ensuite.
        assert_eq!(polls(h.lp.tick(T0 + 60_000).await.unwrap()).await, 0);
    }

    #[tokio::test]
    async fn failed_polls_keep_the_state_and_retry_at_the_next_slot() {
        let h = harness(json!([{ "id": 1 }]));
        sync(
            &h,
            "polling",
            json!({ "url": "https://x", "interval_seconds": 10 }),
        )
        .await;
        h.lp.tick(T0).await.unwrap();
        polls(h.lp.tick(T0).await.unwrap()).await;

        for failure in [
            Err("transport : refus egress".to_string()),
            Ok(HttpResponse {
                status: 503,
                body: json!([{ "id": 2 }]),
            }),
            Ok(HttpResponse {
                status: 200,
                body: json!({ "not": "an array" }),
            }),
        ] {
            h.http.set(failure);
            let now = next_fire(&h).await.unwrap();
            assert_eq!(polls(h.lp.tick(now).await.unwrap()).await, 0);
        }
        h.http.set(Ok(HttpResponse {
            status: 200,
            body: json!([{ "id": 1 }, { "id": 2 }]),
        }));
        let now = next_fire(&h).await.unwrap();
        assert_eq!(polls(h.lp.tick(now).await.unwrap()).await, 1);
    }

    #[tokio::test]
    async fn a_poll_overtaken_by_a_newer_one_dispatches_nothing() {
        let h = harness(json!([]));
        sync(
            &h,
            "polling",
            json!({ "url": "https://x", "interval_seconds": 10 }),
        )
        .await;
        h.lp.tick(T0).await.unwrap();
        polls(h.lp.tick(T0).await.unwrap()).await;
        // Poll réclamé mais pas encore exécuté…
        h.http.set(Ok(HttpResponse {
            status: 200,
            body: json!([{ "id": 9 }]),
        }));
        let slot = h.store.due(i64::MAX, 10).await.unwrap().remove(0);
        let claimed = ScheduleSlot {
            revision: slot.revision + 1,
            ..slot.clone()
        };
        assert!(h
            .store
            .advance(&slot, keep_state(&slot, slot.next_fire_ms))
            .await
            .unwrap());
        // …pendant qu'une autre réplique reprend le nœud : révision changée.
        let current = h.store.due(i64::MAX, 10).await.unwrap().remove(0);
        h.store
            .advance(&current, keep_state(&current, current.next_fire_ms))
            .await
            .unwrap();
        let spec = PollingSpec {
            url: "https://x".into(),
            interval_secs: 10,
            key: "id".into(),
            items_path: None,
        };
        assert_eq!(h.lp.poll(claimed, spec, T0).await, 0);
        assert!(h.runs.requests.lock().unwrap().is_empty());
    }
}

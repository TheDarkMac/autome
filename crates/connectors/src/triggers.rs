//! Déclencheurs planifiés (#227) : paramètres des nœuds `schedule` et
//! `polling`, calcul des échéances et détection des nouveautés d'un polling.
//!
//! Logique pure, sans horloge ni réseau : l'API s'en sert pour **valider** un
//! workflow à l'enregistrement ([`planned_triggers`]), la boucle du worker
//! (`autome_worker::triggers`) pour calculer les échéances et traiter les
//! réponses des polls.
//!
//! ## Paramètres
//!
//! `schedule` — exactement l'un des deux :
//! - `cron` : expression 5 champs ([`CronSchedule`]), évaluée en UTC ;
//!   `timezone` facultatif, seul `UTC` est accepté ;
//! - `interval_seconds` : intervalle fixe.
//!
//! `polling` :
//! - `url` (requis, `http`/`https`) : interrogée en `GET` ;
//! - `interval_seconds` (défaut 60) ;
//! - `items_path` (facultatif) : chemin pointé (`data.items`) du tableau dans
//!   la réponse ; sinon la réponse doit être un tableau ;
//! - `key` (défaut `id`) : champ qui identifie un élément (l'élément entier
//!   s'il en est dépourvu).
//!
//! Intervalles : de [`MIN_INTERVAL_SECS`] à [`MAX_INTERVAL_SECS`].

use std::collections::HashSet;

use autome_core::{NodeKind, ScheduledTrigger, WorkflowGraph};
use serde_json::{json, Value};

use crate::schedule::CronSchedule;

/// Intervalle minimal (s). La granularité réelle est aussi bornée par la
/// période de la boucle du worker.
pub const MIN_INTERVAL_SECS: u64 = 5;
/// Intervalle maximal (s) : 31 jours.
pub const MAX_INTERVAL_SECS: u64 = 31 * 24 * 3_600;
/// Intervalle par défaut d'un polling (s).
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;
/// Éléments d'une réponse de polling pris en compte (les premiers).
pub const MAX_POLL_ITEMS: usize = 1_000;
/// Runs déclenchés au plus par poll ; les nouveautés suivantes restent
/// inédites et partent au poll suivant.
pub const MAX_RUNS_PER_POLL: usize = 100;
/// Clés conservées dans l'état d'un polling.
pub const MAX_SEEN_KEYS: usize = 2 * MAX_POLL_ITEMS;

/// Types de trigger traités par la planification.
pub const PLANNED_TRIGGER_TYPES: [&str; 2] = ["schedule", "polling"];

/// Paramètres validés d'un nœud planifié.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerSpec {
    /// `schedule` + `cron`.
    Cron(CronSchedule),
    /// `schedule` + `interval_seconds`.
    Interval {
        /// Intervalle (s).
        every_secs: u64,
    },
    /// `polling`.
    Polling(PollingSpec),
}

/// Paramètres validés d'un nœud `polling`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollingSpec {
    /// URL interrogée en `GET`.
    pub url: String,
    /// Intervalle entre deux polls (s).
    pub interval_secs: u64,
    /// Champ identifiant un élément.
    pub key: String,
    /// Chemin pointé du tableau dans la réponse (`None` = racine).
    pub items_path: Option<String>,
}

fn interval(params: &Value, default: Option<u64>) -> Result<u64, String> {
    let secs = match params.get("interval_seconds") {
        None | Some(Value::Null) => {
            default.ok_or_else(|| "`interval_seconds` requis".to_string())?
        }
        Some(v) => v
            .as_u64()
            .ok_or_else(|| "`interval_seconds` doit être un entier positif".to_string())?,
    };
    if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&secs) {
        return Err(format!(
            "`interval_seconds` hors bornes ({MIN_INTERVAL_SECS} à {MAX_INTERVAL_SECS})"
        ));
    }
    Ok(secs)
}

fn optional_str<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, String> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim())),
        Some(_) => Err(format!("`{name}` doit être une chaîne")),
    }
}

impl TriggerSpec {
    /// Paramètres d'un nœud trigger de type `trigger_type`. `Ok(None)` si le
    /// type n'est pas planifié (webhook, manual…).
    pub fn parse(trigger_type: &str, params: &Value) -> Result<Option<Self>, String> {
        match trigger_type {
            "schedule" => Self::parse_schedule(params).map(Some),
            "polling" => Self::parse_polling(params).map(|p| Some(Self::Polling(p))),
            _ => Ok(None),
        }
    }

    fn parse_schedule(params: &Value) -> Result<Self, String> {
        if let Some(tz) = optional_str(params, "timezone")? {
            if !tz.eq_ignore_ascii_case("UTC") {
                return Err(format!(
                    "fuseau `{tz}` non pris en charge : les expressions cron sont évaluées en UTC"
                ));
            }
        }
        let cron = optional_str(params, "cron")?;
        let has_interval = !matches!(params.get("interval_seconds"), None | Some(Value::Null));
        match (cron, has_interval) {
            (Some(_), true) => Err("`cron` et `interval_seconds` sont exclusifs".into()),
            (None, false) => Err("`cron` ou `interval_seconds` requis".into()),
            (None, true) => Ok(Self::Interval {
                every_secs: interval(params, None)?,
            }),
            (Some(expr), false) => {
                let cron = CronSchedule::parse(expr)?;
                if cron.next_after(0).is_none() {
                    return Err(format!("cron `{expr}` : aucune date ne correspond"));
                }
                Ok(Self::Cron(cron))
            }
        }
    }

    fn parse_polling(params: &Value) -> Result<PollingSpec, String> {
        let url = optional_str(params, "url")?.ok_or("`url` requis")?;
        let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
        if !matches!(scheme.as_deref(), Some("http" | "https")) {
            return Err(format!("`url` doit être en http(s) : `{url}`"));
        }
        Ok(PollingSpec {
            url: url.to_string(),
            interval_secs: interval(params, Some(DEFAULT_POLL_INTERVAL_SECS))?,
            key: optional_str(params, "key")?.unwrap_or("id").to_string(),
            items_path: optional_str(params, "items_path")?.map(str::to_string),
        })
    }

    /// Prochaine échéance (ms Unix) strictement après `after_ms`. `None` si
    /// l'expression cron ne se déclenche plus.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let every = |secs: u64| after_ms.checked_add(i64::try_from(secs).ok()?.checked_mul(1_000)?);
        match self {
            Self::Cron(cron) => cron.next_after(after_ms),
            Self::Interval { every_secs } => every(*every_secs),
            Self::Polling(p) => every(p.interval_secs),
        }
    }
}

/// Nœuds planifiés de `workflow`, paramètres validés. `Err` nomme le premier
/// nœud invalide — l'API répond alors 400.
pub fn planned_triggers(workflow: &WorkflowGraph) -> Result<Vec<ScheduledTrigger>, String> {
    let mut planned = Vec::new();
    for node in &workflow.nodes {
        let NodeKind::Trigger { trigger_type } = &node.kind else {
            continue;
        };
        if TriggerSpec::parse(trigger_type, &node.params)
            .map_err(|e| format!("nœud `{}` ({trigger_type}) : {e}", node.id.0))?
            .is_some()
        {
            planned.push(ScheduledTrigger {
                node_id: node.id.0.clone(),
                trigger_type: trigger_type.clone(),
                params: node.params.clone(),
            });
        }
    }
    Ok(planned)
}

impl PollingSpec {
    /// Éléments de la réponse d'un poll (au plus [`MAX_POLL_ITEMS`]).
    pub fn items(&self, body: &Value) -> Result<Vec<Value>, String> {
        let mut target = body;
        if let Some(path) = &self.items_path {
            for segment in path.split('.') {
                target = target
                    .get(segment)
                    .ok_or_else(|| format!("`items_path` : `{path}` absent de la réponse"))?;
            }
        }
        let items = target.as_array().ok_or_else(|| match &self.items_path {
            Some(path) => format!("`items_path` : `{path}` n'est pas un tableau"),
            None => "la réponse n'est pas un tableau JSON (renseigner `items_path`)".to_string(),
        })?;
        Ok(items.iter().take(MAX_POLL_ITEMS).cloned().collect())
    }

    /// Nouveautés d'un poll et nouvel état.
    ///
    /// L'état `{ "seen": [clés…] }` liste les éléments déjà traités. **Premier
    /// poll** (état sans `seen`) : référence — tous les éléments présents sont
    /// marqués vus, aucun run (un nouveau workflow ne rejoue pas l'existant).
    /// Ensuite, les éléments inédits sont renvoyés, au plus
    /// [`MAX_RUNS_PER_POLL`], et seuls ceux-là deviennent vus. Les clés des
    /// éléments encore présents sont conservées en premier, puis les plus
    /// récentes des autres, dans la limite de [`MAX_SEEN_KEYS`].
    pub fn step(&self, state: &Value, items: &[Value]) -> (Vec<Value>, Value) {
        let previous: Option<Vec<String>> = state.get("seen").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
        let baseline = previous.is_none();
        let previous = previous.unwrap_or_default();
        let known: HashSet<&str> = previous.iter().map(String::as_str).collect();

        let mut fresh = Vec::new();
        let mut present = Vec::new();
        let mut present_set = HashSet::new();
        for item in items {
            let key = item_key(item, &self.key);
            if !present_set.insert(key.clone()) {
                continue;
            }
            if baseline || known.contains(key.as_str()) {
                present.push(key);
            } else if fresh.len() < MAX_RUNS_PER_POLL {
                fresh.push(item.clone());
                present.push(key);
            }
        }
        let mut seen = present;
        let kept: HashSet<String> = seen.iter().cloned().collect();
        let older: Vec<&String> = previous.iter().filter(|k| !kept.contains(*k)).collect();
        let room = MAX_SEEN_KEYS.saturating_sub(seen.len());
        seen.extend(
            older[older.len().saturating_sub(room)..]
                .iter()
                .map(|k| (*k).clone()),
        );
        seen.truncate(MAX_SEEN_KEYS);
        (fresh, json!({ "seen": seen }))
    }
}

/// Clé d'un élément : valeur JSON de son champ `key`, sinon l'élément entier.
pub fn item_key(item: &Value, key: &str) -> String {
    match item.get(key) {
        Some(v) => v.to_string(),
        None => item.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{Node, NodeId, WorkflowId};

    fn parse(kind: &str, params: Value) -> Result<Option<TriggerSpec>, String> {
        TriggerSpec::parse(kind, &params)
    }

    #[test]
    fn schedule_accepts_cron_or_interval() {
        assert!(matches!(
            parse(
                "schedule",
                json!({ "cron": "*/5 * * * *", "timezone": "UTC" })
            ),
            Ok(Some(TriggerSpec::Cron(_)))
        ));
        assert_eq!(
            parse("schedule", json!({ "interval_seconds": 30 })),
            Ok(Some(TriggerSpec::Interval { every_secs: 30 }))
        );
        // Champ vide de l'éditeur = absent.
        assert!(parse(
            "schedule",
            json!({ "cron": "0 * * * *", "interval_seconds": null })
        )
        .is_ok());
    }

    #[test]
    fn schedule_rejects_invalid_parameters() {
        for (params, needle) in [
            (json!({}), "requis"),
            (json!(null), "requis"),
            (json!({ "cron": "" }), "requis"),
            (json!({ "cron": "* * *" }), "5 champs"),
            (json!({ "cron": "61 * * * *" }), "hors bornes"),
            (json!({ "cron": "0 0 31 2 *" }), "aucune date"),
            (
                json!({ "cron": "* * * * *", "interval_seconds": 60 }),
                "exclusifs",
            ),
            (
                json!({ "cron": "* * * * *", "timezone": "Europe/Paris" }),
                "UTC",
            ),
            (json!({ "interval_seconds": 1 }), "hors bornes"),
            (json!({ "interval_seconds": -5 }), "entier positif"),
            (json!({ "interval_seconds": "60" }), "entier positif"),
            (json!({ "cron": 5 }), "chaîne"),
        ] {
            let err = parse("schedule", params.clone()).unwrap_err();
            assert!(err.contains(needle), "{params} : {err}");
        }
    }

    #[test]
    fn polling_parameters_have_defaults_and_are_validated() {
        assert_eq!(
            parse("polling", json!({ "url": "https://api.example/items" })),
            Ok(Some(TriggerSpec::Polling(PollingSpec {
                url: "https://api.example/items".into(),
                interval_secs: DEFAULT_POLL_INTERVAL_SECS,
                key: "id".into(),
                items_path: None,
            })))
        );
        assert!(parse("polling", json!({}))
            .unwrap_err()
            .contains("`url` requis"));
        assert!(parse("polling", json!({ "url": "file:///etc/passwd" }))
            .unwrap_err()
            .contains("http(s)"));
        assert!(parse(
            "polling",
            json!({ "url": "http://x", "interval_seconds": 2 })
        )
        .unwrap_err()
        .contains("hors bornes"));
    }

    #[test]
    fn other_trigger_types_are_not_planned() {
        assert_eq!(parse("webhook", json!({})), Ok(None));
        assert_eq!(parse("manual", json!(null)), Ok(None));
    }

    #[test]
    fn next_after_for_each_kind() {
        let cron = parse("schedule", json!({ "cron": "0 * * * *" }))
            .unwrap()
            .unwrap();
        assert_eq!(cron.next_after(1_000), Some(3_600_000));
        let every = parse("schedule", json!({ "interval_seconds": 30 }))
            .unwrap()
            .unwrap();
        assert_eq!(every.next_after(1_000), Some(31_000));
        let poll = parse(
            "polling",
            json!({ "url": "http://x", "interval_seconds": 10 }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(poll.next_after(0), Some(10_000));
        assert_eq!(every.next_after(i64::MAX), None);
    }

    fn node(id: &str, kind: NodeKind, params: Value) -> Node {
        Node {
            id: NodeId::new(id),
            name: id.into(),
            kind,
            params,
            position: None,
        }
    }

    fn trigger(id: &str, trigger_type: &str, params: Value) -> Node {
        node(
            id,
            NodeKind::Trigger {
                trigger_type: trigger_type.into(),
            },
            params,
        )
    }

    fn workflow(nodes: Vec<Node>) -> WorkflowGraph {
        WorkflowGraph {
            id: WorkflowId::new("wf"),
            name: "wf".into(),
            nodes,
            edges: vec![],
        }
    }

    #[test]
    fn planned_triggers_lists_schedule_and_polling_nodes() {
        let wf = workflow(vec![
            trigger("hook", "webhook", json!({})),
            trigger("cron", "schedule", json!({ "cron": "0 * * * *" })),
            trigger("poll", "polling", json!({ "url": "https://x" })),
            node(
                "act",
                NodeKind::Action {
                    action_type: "http_request".into(),
                },
                json!({ "cron": "invalid but not a trigger" }),
            ),
        ]);
        let planned = planned_triggers(&wf).unwrap();
        let ids: Vec<_> = planned.iter().map(|t| t.node_id.as_str()).collect();
        assert_eq!(ids, ["cron", "poll"]);
        assert_eq!(planned[0].trigger_type, "schedule");
        assert_eq!(planned[0].params, json!({ "cron": "0 * * * *" }));
    }

    #[test]
    fn planned_triggers_names_the_invalid_node() {
        let wf = workflow(vec![trigger(
            "nightly",
            "schedule",
            json!({ "cron": "bad" }),
        )]);
        let err = planned_triggers(&wf).unwrap_err();
        assert!(err.starts_with("nœud `nightly` (schedule) : "), "{err}");
    }

    fn polling(items_path: Option<&str>) -> PollingSpec {
        PollingSpec {
            url: "https://x".into(),
            interval_secs: 60,
            key: "id".into(),
            items_path: items_path.map(str::to_string),
        }
    }

    #[test]
    fn items_come_from_the_root_or_items_path() {
        let root = json!([{ "id": 1 }]);
        assert_eq!(
            polling(None).items(&root).unwrap(),
            vec![json!({ "id": 1 })]
        );
        let nested = json!({ "data": { "items": [{ "id": 2 }] } });
        assert_eq!(
            polling(Some("data.items")).items(&nested).unwrap(),
            vec![json!({ "id": 2 })]
        );
        assert!(polling(None)
            .items(&nested)
            .unwrap_err()
            .contains("items_path"));
        assert!(polling(Some("data.nope"))
            .items(&nested)
            .unwrap_err()
            .contains("absent"));
        assert!(polling(Some("data"))
            .items(&nested)
            .unwrap_err()
            .contains("pas un tableau"));
        let many = Value::Array((0..2_000).map(|i| json!({ "id": i })).collect());
        assert_eq!(polling(None).items(&many).unwrap().len(), MAX_POLL_ITEMS);
    }

    fn ids(items: &[Value]) -> Vec<i64> {
        items.iter().map(|i| i["id"].as_i64().unwrap()).collect()
    }

    fn batch(range: impl IntoIterator<Item = i64>) -> Vec<Value> {
        range.into_iter().map(|i| json!({ "id": i })).collect()
    }

    #[test]
    fn first_poll_is_a_baseline_then_only_new_items_fire() {
        let spec = polling(None);
        let (fresh, state) = spec.step(&json!({}), &batch([1, 2]));
        assert!(fresh.is_empty(), "premier poll : référence sans run");
        assert_eq!(state, json!({ "seen": ["1", "2"] }));

        let (fresh, state) = spec.step(&state, &batch([3, 1, 2]));
        assert_eq!(ids(&fresh), [3]);
        let (fresh, _) = spec.step(&state, &batch([3, 2, 1]));
        assert!(fresh.is_empty());
    }

    #[test]
    fn baseline_on_an_empty_source_still_records_the_baseline() {
        let spec = polling(None);
        let (_, state) = spec.step(&json!({}), &[]);
        assert_eq!(state, json!({ "seen": [] }));
        let (fresh, _) = spec.step(&state, &batch([7]));
        assert_eq!(ids(&fresh), [7]);
    }

    #[test]
    fn runs_per_poll_are_capped_and_the_rest_stays_new() {
        let spec = polling(None);
        let (_, state) = spec.step(&json!({}), &[]);
        let items = batch(0..150);
        let (fresh, state) = spec.step(&state, &items);
        assert_eq!(fresh.len(), MAX_RUNS_PER_POLL);
        let (fresh, state) = spec.step(&state, &items);
        assert_eq!(ids(&fresh), (100..150).collect::<Vec<_>>());
        assert!(spec.step(&state, &items).0.is_empty());
    }

    #[test]
    fn duplicates_within_a_response_fire_once_and_keyless_items_use_the_whole_value() {
        let spec = polling(None);
        let (_, state) = spec.step(&json!({}), &[]);
        let items = vec![
            json!({ "id": 1 }),
            json!({ "id": 1 }),
            json!({ "x": 1 }),
            json!({ "x": 1 }),
        ];
        let (fresh, _) = spec.step(&state, &items);
        assert_eq!(fresh, vec![json!({ "id": 1 }), json!({ "x": 1 })]);
    }

    #[test]
    fn seen_keys_are_bounded_and_keep_present_items_first() {
        let spec = polling(None);
        let (_, mut state) = spec.step(&json!({}), &batch(0..1_000));
        // Une source qui défile : chaque poll apporte 100 nouveautés.
        for round in 1..=30 {
            let window = batch(round * 100..round * 100 + 1_000);
            let (fresh, next) = spec.step(&state, &window);
            assert_eq!(fresh.len(), 100, "round {round}");
            state = next;
        }
        let seen = state["seen"].as_array().unwrap();
        assert_eq!(seen.len(), MAX_SEEN_KEYS);
        // Les éléments encore présents ne repartent jamais.
        let window = batch(3_000..4_000);
        assert!(spec.step(&state, &window).0.is_empty());
    }
}

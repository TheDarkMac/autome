//! Exécution séquentielle d'un workflow validé (F2.2).
//!
//! Parcourt le [`Dag`] (F2.1) dans l'ordre topologique (F2.2.a), exécute
//! chaque node via un [`NodeRunner`] injecté, accumule les sorties dans un
//! [`RunContext`] (F2.2.b) et produit un [`RunReport`] par node (F2.2.c).
//!
//! Le moteur reste **agnostique** : il ne sait pas *comment* un node
//! s'exécute (HTTP, Slack, …) — c'est le `NodeRunner` qui le décide. Les
//! implémentations concrètes relèvent des domaines 3/4.
//!
//! Branch-aware (F2.3) : `If`/`Switch` sont évalués par le moteur et
//! n'activent que la branche retenue. `Loop`/`While` (F2.4) sont aussi
//! évalués par le moteur : ils délèguent le corps au runner par itération
//! (élément/index exposés au contexte) et agrègent les sorties en tableau.
//! Fail-fast : au premier échec, le reste est `Skipped`.
//!
//! [`run_parallel`] (F2.5) ordonnance le même graphe de façon concurrente
//! (fan-out/fan-in, concurrence bornée) avec une sémantique identique.
//!
//! Reprise (#239) : [`resume_sequential`] exécute le parcours séquentiel à
//! partir d'un [`Cursor`] (nœuds traités, contexte, branches actives) et
//! consulte un [`RunObserver`] avant et après chaque nœud — le worker y
//! enregistre sa progression et peut interrompre le run entre deux nœuds.
//!
//! Mapping (F2.6) : les `params` des nodes runner sont résolus/templatés
//! contre le contexte avant exécution (cf. [`crate::mapping`]).
//!
//! Gestion d'erreurs (F2.7) : politique par node via `params._error`
//! (retry + backoff, timeout, `on_error` = fail | continue | fallback).
//! Un échec **géré** (continue/fallback) n'arrête pas le run ; un échec
//! **non géré** déclenche le fail-fast (downstream `Skipped`). Le statut
//! final reflète uniquement les échecs non gérés.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{mpsc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use autome_core::{Edge, Node, NodeId, NodeKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{condition, Dag, RunStatus};

/// Statut d'un node exécuté dans un run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    /// Exécuté avec succès.
    Succeeded,
    /// A échoué (voir `error`).
    Failed,
    /// Non exécuté car un node précédent a échoué (fail-fast).
    Skipped,
}

/// Résultat d'exécution d'un node.
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    /// Node concerné.
    pub node_id: NodeId,
    /// Statut final du node.
    pub status: StepStatus,
    /// Sortie produite (présente seulement si `Succeeded`).
    pub output: Option<Value>,
    /// Message d'erreur (présent seulement si `Failed`).
    pub error: Option<String>,
}

/// Contexte d'un run : sorties accumulées des nodes déjà exécutés (F2.2.b).
///
/// Un `NodeRunner` peut y lire la sortie d'un prédécesseur. Le *mapping*
/// fin des champs d'entrée relève de F2.6.
#[derive(Debug, Default, Clone)]
pub struct RunContext {
    outputs: HashMap<NodeId, Value>,
}

impl RunContext {
    /// Sortie d'un node déjà exécuté, si disponible.
    pub fn output(&self, id: &NodeId) -> Option<&Value> {
        self.outputs.get(id)
    }

    fn record(&mut self, id: NodeId, value: Value) {
        self.outputs.insert(id, value);
    }

    fn forget(&mut self, id: &NodeId) {
        self.outputs.remove(id);
    }

    /// Injection directe d'une sortie (tests uniquement).
    #[cfg(test)]
    pub(crate) fn record_for_test(&mut self, id: NodeId, value: Value) {
        self.outputs.insert(id, value);
    }
}

/// Stratégie d'exécution d'un node, injectée dans le moteur.
///
/// `Err(message)` => le node est en échec ; le run s'arrête (fail-fast).
pub trait NodeRunner {
    /// Exécute `node`, avec accès en lecture aux sorties précédentes.
    fn run(&self, node: &Node, ctx: &RunContext) -> Result<Value, String>;
}

/// Implémentation de [`NodeRunner`] à partir d'une closure (tests, glue).
impl<F> NodeRunner for F
where
    F: Fn(&Node, &RunContext) -> Result<Value, String>,
{
    fn run(&self, node: &Node, ctx: &RunContext) -> Result<Value, String> {
        self(node, ctx)
    }
}

/// Rapport d'un run : statut global + résultat par node (F2.2.c).
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    /// Statut global du run.
    pub status: RunStatus,
    /// Résultats, dans l'ordre topologique d'exécution.
    pub steps: Vec<StepOutcome>,
}

impl RunReport {
    /// Résultat d'un node donné, si présent dans le run.
    pub fn step(&self, id: &NodeId) -> Option<&StepOutcome> {
        self.steps.iter().find(|s| &s.node_id == id)
    }
}

/// Quelles arêtes sortantes d'un node propagent l'activation.
enum Outflow {
    /// Toutes les arêtes (sortie par défaut : trigger/action/loop/while).
    All,
    /// Seules les arêtes dont `from_port == Some(port)` (if/switch).
    Port(String),
    /// Aucune arête (switch sans branche correspondante ni défaut).
    None,
}

/// Issue de l'exécution d'un node : sa sortie et la propagation choisie.
type NodeResult = Result<(Value, Outflow), String>;

/// Évalue un node `If` : `params.condition` (chaîne) → port `true`/`false`.
fn eval_if(node: &Node, ctx: &RunContext) -> NodeResult {
    let cond = node
        .params
        .get("condition")
        .and_then(Value::as_str)
        .ok_or_else(|| "node `if` : `params.condition` (chaîne) requis".to_string())?;
    let taken = condition::eval_bool(cond, ctx)?;
    let port = if taken { "true" } else { "false" };
    Ok((Value::Bool(taken), Outflow::Port(port.to_string())))
}

/// Évalue un node `Switch` : `params.selector` comparé à `params.cases`
/// (`[{ "value": <json>, "port": "<branche>" }]`), `params.default`
/// optionnel. Sortie = valeur du sélecteur ; propagation = port choisi.
fn eval_switch(node: &Node, ctx: &RunContext) -> NodeResult {
    let selector = node
        .params
        .get("selector")
        .and_then(Value::as_str)
        .ok_or_else(|| "node `switch` : `params.selector` (chaîne) requis".to_string())?;
    let value = condition::resolve(selector, ctx)?;

    let cases = node
        .params
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| "node `switch` : `params.cases` (tableau) requis".to_string())?;

    for case in cases {
        let cv = case
            .get("value")
            .ok_or_else(|| "case `switch` sans champ `value`".to_string())?;
        let port = case
            .get("port")
            .and_then(Value::as_str)
            .ok_or_else(|| "case `switch` sans `port` (chaîne)".to_string())?;
        if condition::json_eq(&value, cv) {
            return Ok((value, Outflow::Port(port.to_string())));
        }
    }
    match node.params.get("default").and_then(Value::as_str) {
        Some(p) => Ok((value, Outflow::Port(p.to_string()))),
        None => Ok((value, Outflow::None)),
    }
}

/// Clé de contexte exposant l'élément courant d'une boucle `Loop`
/// (`‹loop_id›::item`) — lisible par le runner / les conditions.
fn item_key(node_id: &str) -> NodeId {
    NodeId::new(format!("{node_id}::item"))
}

/// Clé de contexte exposant l'index d'itération courant (`‹id›::index`).
fn index_key(node_id: &str) -> NodeId {
    NodeId::new(format!("{node_id}::index"))
}

/// Évalue un node `Loop` : itère sur la collection `params.items`
/// (chemin/opérande résolu en tableau). À chaque itération, l'élément et
/// l'index sont exposés au contexte (`‹id›::item` / `::index`) et le
/// **corps** est délégué au `runner`. Sortie = tableau des sorties par
/// itération (F2.4.c). Fail-fast : une itération en échec stoppe la boucle.
fn eval_loop(node: &Node, ctx: &mut RunContext, runner: &(dyn NodeRunner + Sync)) -> NodeResult {
    let expr = node
        .params
        .get("items")
        .and_then(Value::as_str)
        .ok_or_else(|| "node `loop` : `params.items` (chaîne) requis".to_string())?;
    let items = match condition::resolve(expr, ctx)? {
        Value::Array(a) => a,
        other => {
            return Err(format!(
                "node `loop` : `items` doit être un tableau (reçu : {other})"
            ))
        }
    };

    let id = node.id.0.clone();
    let (ik, xk) = (item_key(&id), index_key(&id));
    let mut results = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        ctx.record(ik.clone(), item);
        ctx.record(xk.clone(), Value::from(i as i64));
        let outcome = runner.run(node, ctx);
        match outcome {
            Ok(out) => results.push(out),
            Err(e) => {
                ctx.forget(&ik);
                ctx.forget(&xk);
                return Err(format!("itération {i} : {e}"));
            }
        }
    }
    ctx.forget(&ik);
    ctx.forget(&xk);
    Ok((Value::Array(results), Outflow::All))
}

/// Borne d'itérations par défaut d'un `While` (garde anti-boucle infinie).
const DEFAULT_MAX_ITERATIONS: u64 = 1000;

/// Évalue un node `While` : tant que `params.condition` est vraie, délègue
/// le corps au `runner`. `params.max_iterations` (défaut
/// [`DEFAULT_MAX_ITERATIONS`]) borne l'exécution : si la condition reste
/// vraie au-delà, c'est une **erreur** (garde anti-boucle infinie, F2.4.b).
/// L'index d'itération est exposé (`‹id›::index`). Sortie = tableau des
/// sorties par itération (F2.4.c).
fn eval_while(node: &Node, ctx: &mut RunContext, runner: &(dyn NodeRunner + Sync)) -> NodeResult {
    let cond = node
        .params
        .get("condition")
        .and_then(Value::as_str)
        .ok_or_else(|| "node `while` : `params.condition` (chaîne) requis".to_string())?
        .to_string();
    let max = node
        .params
        .get("max_iterations")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_ITERATIONS);

    let id = node.id.0.clone();
    let xk = index_key(&id);
    let mut results = Vec::new();
    let mut i: u64 = 0;
    loop {
        ctx.record(xk.clone(), Value::from(i as i64));
        let keep_going = match condition::eval_bool(&cond, ctx) {
            Ok(b) => b,
            Err(e) => {
                ctx.forget(&xk);
                return Err(e);
            }
        };
        if !keep_going {
            break;
        }
        if i >= max {
            ctx.forget(&xk);
            return Err(format!(
                "garde anti-boucle : `max_iterations` ({max}) atteint, condition encore vraie"
            ));
        }
        match runner.run(node, ctx) {
            Ok(out) => results.push(out),
            Err(e) => {
                ctx.forget(&xk);
                return Err(format!("itération {i} : {e}"));
            }
        }
        i += 1;
    }
    ctx.forget(&xk);
    Ok((Value::Array(results), Outflow::All))
}

/// Conduite à tenir quand un node runner échoue (F2.7.c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnError {
    /// Échec non géré : le run s'arrête (fail-fast). Défaut.
    Fail,
    /// Échec toléré : node `Failed`, mais le run continue (sortie par défaut).
    Continue,
    /// Échec routé vers le port `error` (handler/fallback).
    Fallback,
}

/// Politique d'erreur d'un node, lue depuis `params._error` (F2.7).
///
/// ```json
/// { "_error": { "max_attempts": 3, "backoff_ms": 50,
///               "timeout_ms": 2000, "on_error": "fallback" } }
/// ```
struct ErrorPolicy {
    max_attempts: u32,
    backoff_ms: u64,
    timeout_ms: Option<u64>,
    on_error: OnError,
}

impl ErrorPolicy {
    fn from_params(params: &Value) -> Self {
        let e = params.get("_error");
        let get_u64 = |k: &str| e.and_then(|v| v.get(k)).and_then(Value::as_u64);
        let on_error = match e.and_then(|v| v.get("on_error")).and_then(Value::as_str) {
            Some("continue") => OnError::Continue,
            Some("fallback") => OnError::Fallback,
            _ => OnError::Fail,
        };
        Self {
            max_attempts: get_u64("max_attempts").unwrap_or(1).max(1) as u32,
            backoff_ms: get_u64("backoff_ms").unwrap_or(0),
            timeout_ms: get_u64("timeout_ms"),
            on_error,
        }
    }
}

/// Évaluation complète d'un node : sortie, propagation, statut, abandon.
struct NodeEval {
    output: Value,
    outflow: Outflow,
    status: StepStatus,
    error: Option<String>,
    /// `true` ⇒ échec non géré : le run s'arrête (fail-fast).
    abort: bool,
}

fn eval_ok(output: Value, outflow: Outflow) -> NodeEval {
    NodeEval {
        output,
        outflow,
        status: StepStatus::Succeeded,
        error: None,
        abort: false,
    }
}

fn eval_fatal(error: String) -> NodeEval {
    NodeEval {
        output: Value::Null,
        outflow: Outflow::None,
        status: StepStatus::Failed,
        error: Some(error),
        abort: true,
    }
}

fn from_control(r: NodeResult) -> NodeEval {
    match r {
        Ok((out, outflow)) => eval_ok(out, outflow),
        Err(e) => eval_fatal(e),
    }
}

/// Une tentative d'exécution du runner, éventuellement bornée par un
/// timeout (F2.7.b).
///
/// Limitation assumée : safe Rust ne préempte pas un thread. Le timeout
/// est une **date-limite d'attente** : passé le délai, l'erreur de
/// timeout est produite ; le thread du runner est néanmoins joint avant
/// le retour (un runner non coopératif peut donc dépasser le délai en
/// temps réel, mais son résultat est ignoré).
fn run_once(
    node: &Node,
    ctx: &RunContext,
    runner: &(dyn NodeRunner + Sync),
    timeout_ms: Option<u64>,
) -> Result<Value, String> {
    match timeout_ms {
        None => runner.run(node, ctx),
        Some(ms) => thread::scope(|s| {
            let (tx, rx) = mpsc::channel();
            s.spawn(move || {
                let _ = tx.send(runner.run(node, ctx));
            });
            match rx.recv_timeout(Duration::from_millis(ms)) {
                Ok(r) => r,
                Err(_) => Err(format!("timeout : node `{}` a dépassé {ms} ms", node.id.0)),
            }
        }),
    }
}

/// Exécute le runner avec retry (F2.7.a) + timeout (F2.7.b).
fn run_with_policy(
    node: &Node,
    ctx: &RunContext,
    runner: &(dyn NodeRunner + Sync),
    policy: &ErrorPolicy,
) -> Result<Value, String> {
    let mut last = String::from("échec");
    for attempt in 1..=policy.max_attempts {
        match run_once(node, ctx, runner, policy.timeout_ms) {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = e;
                if attempt < policy.max_attempts && policy.backoff_ms > 0 {
                    thread::sleep(Duration::from_millis(policy.backoff_ms));
                }
            }
        }
    }
    Err(last)
}

/// Évalue un node selon son type (commun séquentiel/parallèle).
fn evaluate_node(node: &Node, ctx: &mut RunContext, runner: &(dyn NodeRunner + Sync)) -> NodeEval {
    match &node.kind {
        // Les nodes de contrôle évaluent eux-mêmes leurs expressions brutes.
        NodeKind::If => from_control(eval_if(node, ctx)),
        NodeKind::Switch => from_control(eval_switch(node, ctx)),
        NodeKind::Loop => from_control(eval_loop(node, ctx, runner)),
        NodeKind::While => from_control(eval_while(node, ctx, runner)),
        // Chemin runner : params résolus/templatés (F2.6), puis politique
        // d'erreur retry/timeout/fallback (F2.7).
        _ => {
            let policy = ErrorPolicy::from_params(&node.params);
            let resolved = match crate::mapping::resolve_params(node, ctx) {
                Ok(v) => v,
                Err(e) => return eval_fatal(format!("mapping des params : {e}")),
            };
            let mut node = node.clone();
            node.params = resolved;
            match run_with_policy(&node, ctx, runner, &policy) {
                Ok(out) => eval_ok(out, Outflow::All),
                Err(e) => match policy.on_error {
                    OnError::Fail => NodeEval {
                        output: Value::Null,
                        outflow: Outflow::None,
                        status: StepStatus::Failed,
                        error: Some(e),
                        abort: true,
                    },
                    OnError::Continue => NodeEval {
                        output: Value::Null,
                        outflow: Outflow::All,
                        status: StepStatus::Failed,
                        error: Some(e),
                        abort: false,
                    },
                    OnError::Fallback => NodeEval {
                        output: serde_json::json!({ "error": e.clone() }),
                        outflow: Outflow::Port("error".to_string()),
                        status: StepStatus::Failed,
                        error: Some(e),
                        abort: false,
                    },
                },
            }
        }
    }
}

/// L'arête `e` propage-t-elle l'activation, vu le `outflow` du node source ?
fn edge_taken(outflow: &Outflow, e: &Edge) -> bool {
    match outflow {
        Outflow::All => true,
        Outflow::Port(p) => e.from_port.as_deref() == Some(p.as_str()),
        Outflow::None => false,
    }
}

/// Exécute le DAG séquentiellement dans l'ordre topologique.
///
/// Branch-aware (F2.3) : `If`/`Switch` sont évalués par le moteur (pas le
/// runner) et n'activent que les nodes du port retenu ; les nodes jamais
/// activés sont `Skipped`. Fail-fast : au premier échec, le reste est
/// `Skipped` et le run est `Failed`.
pub fn run_sequential(dag: &Dag, runner: &(dyn NodeRunner + Sync)) -> RunReport {
    match resume_sequential(dag, runner, Cursor::start(dag), &Unobserved) {
        Ok(Execution::Finished(report)) => report,
        _ => unreachable!("un parcours sans observateur va à son terme"),
    }
}

/// Point de reprise d'un parcours séquentiel (#239) : état du moteur entre
/// deux nœuds. Sérialisable : le worker l'enregistre après chaque nœud et
/// reprend un run interrompu là où il s'était arrêté, sans rien rejouer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    /// Empreinte du graphe parcouru ([`Dag::fingerprint`]).
    graph: String,
    /// Issues des nœuds déjà traités, préfixe de l'ordre topologique.
    steps: Vec<StepRecord>,
    /// Valeurs du contexte par nœud, y compris celles d'un échec géré. Seule
    /// copie des sorties : une sortie `null` y reste `null` (un
    /// `Option<Value>` sérialisé la rendrait absente).
    context: BTreeMap<String, Value>,
    /// Nœuds atteints par une arête prise (ou racine), pas forcément traités.
    active: BTreeSet<String>,
    /// Un échec non géré a eu lieu : le reste sera `Skipped`.
    failed: bool,
}

/// Issue d'un nœud traité, sans sa sortie (lue dans le contexte).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StepRecord {
    node: String,
    status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl StepRecord {
    fn of(step: &StepOutcome) -> Self {
        Self {
            node: step.node_id.0.clone(),
            status: step.status,
            error: step.error.clone(),
        }
    }

    fn outcome(&self, context: &BTreeMap<String, Value>) -> StepOutcome {
        StepOutcome {
            node_id: NodeId::new(self.node.clone()),
            status: self.status,
            output: (self.status == StepStatus::Succeeded)
                .then(|| context.get(&self.node).cloned().unwrap_or_default()),
            error: self.error.clone(),
        }
    }
}

impl Cursor {
    /// Curseur d'un run qui démarre : rien de traité, racine active.
    pub fn start(dag: &Dag) -> Self {
        Self {
            graph: dag.fingerprint(),
            steps: Vec::new(),
            context: BTreeMap::new(),
            active: BTreeSet::from([dag.root().id.0.clone()]),
            failed: false,
        }
    }

    /// Nombre de nœuds déjà traités (exécutés ou sautés).
    pub fn done(&self) -> usize {
        self.steps.len()
    }

    /// Issues des nœuds déjà traités.
    pub fn steps(&self) -> Vec<StepOutcome> {
        self.steps
            .iter()
            .map(|r| r.outcome(&self.context))
            .collect()
    }

    /// Sorties des nœuds réussis (celles qu'un checkpoint expose).
    pub fn outputs(&self) -> BTreeMap<String, Value> {
        self.steps
            .iter()
            .filter(|r| r.status == StepStatus::Succeeded)
            .map(|r| {
                let value = self.context.get(&r.node).cloned().unwrap_or_default();
                (r.node.clone(), value)
            })
            .collect()
    }

    /// Le curseur a-t-il été produit par ce graphe ? Faux si le workflow a
    /// changé depuis (nodes, paramètres ou arêtes).
    pub fn fits(&self, dag: &Dag) -> bool {
        self.graph == dag.fingerprint()
    }

    fn push(&mut self, steps: &mut Vec<StepOutcome>, step: StepOutcome) {
        self.steps.push(StepRecord::of(&step));
        steps.push(step);
    }
}

/// Décision d'un [`RunObserver`] avant un nœud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Exécuter le nœud.
    Continue,
    /// Interrompre le run avant le nœud (rien de lui n'est exécuté).
    Stop,
}

/// Observateur d'un parcours séquentiel (#239), appelé autour de chaque
/// nœud **exécuté** (un nœud `Skipped` ne l'est pas).
pub trait RunObserver {
    /// `node` va s'exécuter ; `cursor` ne le compte pas encore.
    fn before_node(&self, node: &NodeId, cursor: &Cursor) -> Control;
    /// Un nœud vient d'être traité ; `cursor` le compte.
    fn after_node(&self, cursor: &Cursor);
}

/// Observateur neutre de [`run_sequential`].
struct Unobserved;

impl RunObserver for Unobserved {
    fn before_node(&self, _: &NodeId, _: &Cursor) -> Control {
        Control::Continue
    }
    fn after_node(&self, _: &Cursor) {}
}

/// Issue d'un parcours repris ou observé.
#[derive(Debug, Clone, PartialEq)]
pub enum Execution {
    /// Le run est allé à son terme.
    Finished(RunReport),
    /// Interrompu par l'observateur avant `next`, qui n'a pas été exécuté.
    Interrupted {
        /// Point de reprise (à enregistrer pour continuer plus tard).
        cursor: Cursor,
        /// Nœud qui aurait été exécuté.
        next: NodeId,
    },
}

/// Exécute le DAG séquentiellement **à partir de `cursor`** (le
/// [`Cursor::start`] d'un run neuf, ou celui d'un run interrompu), en
/// consultant `observer` autour de chaque nœud. Mêmes sémantiques que
/// [`run_sequential`] : un run repris produit le même rapport qu'un run
/// d'une traite.
///
/// `Err` si le curseur ne correspond pas à ce graphe ([`Cursor::fits`]).
pub fn resume_sequential(
    dag: &Dag,
    runner: &(dyn NodeRunner + Sync),
    mut cursor: Cursor,
    observer: &dyn RunObserver,
) -> Result<Execution, String> {
    let order = dag.topological_order();
    let prefix_ok = cursor.steps.len() <= order.len()
        && cursor
            .steps
            .iter()
            .zip(&order)
            .all(|(step, id)| step.node == id.0);
    if !cursor.fits(dag) || !prefix_ok {
        return Err("point de reprise d'un autre graphe (workflow modifié)".into());
    }
    let mut ctx = RunContext {
        outputs: cursor
            .context
            .iter()
            .map(|(k, v)| (NodeId::new(k.clone()), v.clone()))
            .collect(),
    };
    let mut steps = cursor.steps();

    for id in &order[cursor.steps.len()..] {
        if cursor.failed || !cursor.active.contains(&id.0) {
            cursor.push(
                &mut steps,
                StepOutcome {
                    node_id: id.clone(),
                    status: StepStatus::Skipped,
                    output: None,
                    error: None,
                },
            );
            continue;
        }
        if observer.before_node(id, &cursor) == Control::Stop {
            return Ok(Execution::Interrupted {
                cursor,
                next: id.clone(),
            });
        }

        let node = dag.node(id).expect("node issu du DAG");
        let ev = evaluate_node(node, &mut ctx, runner);

        // La sortie est enregistrée même en cas d'échec géré (un handler
        // sur le port `error` peut vouloir lire `{ "error": … }`).
        ctx.record(id.clone(), ev.output.clone());
        cursor.context.insert(id.0.clone(), ev.output.clone());
        let output = (ev.status == StepStatus::Succeeded).then_some(ev.output);
        cursor.push(
            &mut steps,
            StepOutcome {
                node_id: id.clone(),
                status: ev.status,
                output,
                error: ev.error,
            },
        );

        if ev.abort {
            cursor.failed = true; // échec non géré → fail-fast (F2.7.d)
        } else {
            for e in dag.successors(id) {
                if edge_taken(&ev.outflow, e) {
                    cursor.active.insert(e.to.0.clone());
                }
            }
        }
        observer.after_node(&cursor);
    }

    let status = if cursor.failed {
        RunStatus::Failed
    } else {
        RunStatus::Succeeded
    };
    Ok(Execution::Finished(RunReport { status, steps }))
}

/// Un nœud interrompu **pendant** son exécution peut-il être rejoué à la
/// reprise (#239) ? Son effet a pu se produire ou non : il ne l'est que s'il
/// est sans effet de bord (trigger, `if`, `switch`), ou si sa politique
/// `_error` autorise déjà plusieurs tentatives (`max_attempts > 1`, F2.7) —
/// l'auteur du workflow a alors accepté qu'il s'exécute plus d'une fois.
/// `loop` / `while` (corps délégué au runner) ne le sont jamais.
pub fn rerunnable(node: &Node) -> bool {
    match node.kind {
        NodeKind::Trigger { .. } | NodeKind::If | NodeKind::Switch => true,
        NodeKind::Loop | NodeKind::While => false,
        NodeKind::Action { .. } => ErrorPolicy::from_params(&node.params).max_attempts > 1,
    }
}

/// État partagé de l'ordonnanceur parallèle (F2.5).
struct Sched {
    /// Sorties accumulées (contexte partagé).
    outputs: HashMap<NodeId, Value>,
    /// Arêtes entrantes pas encore résolues, par node.
    pending: HashMap<NodeId, usize>,
    /// Le node a-t-il été atteint par une arête « prise » ?
    activated: HashMap<NodeId, bool>,
    /// Résultat final par node.
    outcome: HashMap<NodeId, StepOutcome>,
    /// Nodes prêts à être exécutés.
    ready: Vec<NodeId>,
    /// Nodes en cours d'exécution.
    in_flight: usize,
    /// Un node a échoué → on n'ordonnance plus rien (fail-fast).
    aborted: bool,
}

fn skipped(id: &NodeId) -> StepOutcome {
    StepOutcome {
        node_id: id.clone(),
        status: StepStatus::Skipped,
        output: None,
        error: None,
    }
}

/// Résout une arête entrante de `target` : décrémente son compteur et,
/// quand il atteint 0, met le node prêt (s'il est activé) ou le marque
/// `Skipped` en cascade (branche non prise / fan-in non atteint).
fn resolve_edge(s: &mut Sched, dag: &Dag, target: &NodeId) {
    let p = s.pending.get_mut(target).expect("node indexé");
    *p -= 1;
    if *p != 0 || s.aborted {
        return;
    }
    if *s.activated.get(target).expect("node indexé") {
        s.ready.push(target.clone());
    } else {
        s.outcome
            .entry(target.clone())
            .or_insert_with(|| skipped(target));
        let succ: Vec<NodeId> = dag
            .successors(target)
            .iter()
            .map(|e| e.to.clone())
            .collect();
        for to in succ {
            resolve_edge(s, dag, &to);
        }
    }
}

/// Acquiert un node prêt (par ordre topologique, déterministe) ou rend
/// `None` quand il n'y a plus rien à faire.
fn acquire(
    sched: &Mutex<Sched>,
    cv: &Condvar,
    topo: &HashMap<NodeId, usize>,
    total: usize,
) -> Option<(NodeId, RunContext)> {
    let mut g = sched.lock().expect("mutex");
    loop {
        let done = g.outcome.len() == total
            || (g.aborted && g.in_flight == 0)
            || (g.ready.is_empty() && g.in_flight == 0);
        if done {
            cv.notify_all();
            return None;
        }
        if !g.aborted {
            if let Some(pos) = g
                .ready
                .iter()
                .enumerate()
                .min_by_key(|(_, id)| topo[*id])
                .map(|(i, _)| i)
            {
                let id = g.ready.remove(pos);
                g.in_flight += 1;
                let ctx = RunContext {
                    outputs: g.outputs.clone(),
                };
                return Some((id, ctx));
            }
        }
        g = cv.wait(g).expect("mutex");
    }
}

/// Valide l'évaluation d'un node et débloque ses successeurs.
fn commit(sched: &Mutex<Sched>, cv: &Condvar, dag: &Dag, id: &NodeId, ev: NodeEval) {
    let mut g = sched.lock().expect("mutex");
    g.outputs.insert(id.clone(), ev.output.clone());
    let output = (ev.status == StepStatus::Succeeded).then_some(ev.output);
    g.outcome.insert(
        id.clone(),
        StepOutcome {
            node_id: id.clone(),
            status: ev.status,
            output,
            error: ev.error,
        },
    );

    if ev.abort {
        g.aborted = true; // échec non géré → fail-fast
    } else {
        let succ: Vec<Edge> = dag.successors(id).to_vec();
        for e in &succ {
            if edge_taken(&ev.outflow, e) {
                *g.activated.get_mut(&e.to).expect("node indexé") = true;
            }
        }
        for e in &succ {
            resolve_edge(&mut g, dag, &e.to);
        }
    }
    g.in_flight -= 1;
    cv.notify_all();
}

/// Exécute le DAG **en parallèle** : fan-out concurrent des branches
/// indépendantes (F2.5.a), fan-in implicite via le comptage d'arêtes
/// entrantes (F2.5.b), concurrence bornée par `max_concurrency` workers
/// (F2.5.c, min 1). Mêmes sémantiques que [`run_sequential`] (branches,
/// boucles, fail-fast) ; rapport stable (ordre topologique).
pub fn run_parallel(
    dag: &Dag,
    runner: &(dyn NodeRunner + Sync),
    max_concurrency: usize,
) -> RunReport {
    let order = dag.topological_order();
    let topo: HashMap<NodeId, usize> = order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), i))
        .collect();

    let mut pending: HashMap<NodeId, usize> = order.iter().map(|id| (id.clone(), 0usize)).collect();
    for id in &order {
        for e in dag.successors(id) {
            *pending.get_mut(&e.to).expect("node indexé") += 1;
        }
    }
    let mut activated: HashMap<NodeId, bool> = order.iter().map(|id| (id.clone(), false)).collect();
    let root = dag.root().id.clone();
    *activated.get_mut(&root).expect("racine") = true;

    let sched = Mutex::new(Sched {
        outputs: HashMap::new(),
        pending,
        activated,
        outcome: HashMap::new(),
        ready: vec![root],
        in_flight: 0,
        aborted: false,
    });
    let cv = Condvar::new();
    let total = order.len();
    let workers = max_concurrency.max(1);

    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                match acquire(&sched, &cv, &topo, total) {
                    None => break,
                    Some((id, mut ctx)) => {
                        let node = dag.node(&id).expect("node issu du DAG");
                        let ev = evaluate_node(node, &mut ctx, runner);
                        commit(&sched, &cv, dag, &id, ev);
                    }
                }
            });
        }
    });

    let sched = sched.into_inner().expect("mutex");
    let steps: Vec<StepOutcome> = order
        .iter()
        .map(|id| {
            sched
                .outcome
                .get(id)
                .cloned()
                .unwrap_or_else(|| skipped(id))
        })
        .collect();
    // Le run échoue seulement sur un échec **non géré** (abandon) ; les
    // échecs gérés (continue/fallback) laissent le run réussir (F2.7.d).
    let status = if sched.aborted {
        RunStatus::Failed
    } else {
        RunStatus::Succeeded
    };
    RunReport { status, steps }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{Edge, NodeKind, WorkflowGraph, WorkflowId};

    fn n(id: &str) -> Node {
        Node {
            position: None,
            id: NodeId::new(id),
            name: id.into(),
            kind: NodeKind::Action {
                action_type: "noop".into(),
            },
            params: Value::Null,
        }
    }
    fn trig(id: &str) -> Node {
        Node {
            position: None,
            id: NodeId::new(id),
            name: id.into(),
            kind: NodeKind::Trigger {
                trigger_type: "manual".into(),
            },
            params: Value::Null,
        }
    }
    fn e(from: &str, to: &str) -> Edge {
        Edge {
            from: NodeId::new(from),
            to: NodeId::new(to),
            from_port: None,
        }
    }
    fn dag(nodes: Vec<Node>, edges: Vec<Edge>) -> Dag {
        Dag::from_workflow(&WorkflowGraph {
            id: WorkflowId::new("wf"),
            name: "t".into(),
            nodes,
            edges,
        })
        .expect("DAG valide")
    }

    #[test]
    fn runs_all_nodes_in_topological_order_on_success() {
        let d = dag(
            vec![trig("t"), n("a"), n("b")],
            vec![e("t", "a"), e("a", "b")],
        );
        let runner = |node: &Node, _: &RunContext| Ok(serde_json::json!({ "id": node.id.0 }));
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        let seq: Vec<String> = report.steps.iter().map(|s| s.node_id.0.clone()).collect();
        assert_eq!(seq, vec!["t", "a", "b"]);
        assert!(report
            .steps
            .iter()
            .all(|s| s.status == StepStatus::Succeeded));
    }

    #[test]
    fn context_exposes_predecessor_outputs() {
        let d = dag(vec![trig("t"), n("a")], vec![e("t", "a")]);
        let runner = |node: &Node, ctx: &RunContext| {
            if node.id.0 == "a" {
                // La sortie du trigger doit être visible.
                let upstream = ctx.output(&NodeId::new("t")).expect("sortie de t");
                assert_eq!(upstream, &serde_json::json!("hello"));
            }
            Ok(serde_json::json!("hello"))
        };
        let report = run_sequential(&d, &runner);
        assert_eq!(report.status, RunStatus::Succeeded);
    }

    #[test]
    fn fail_fast_marks_downstream_skipped() {
        let d = dag(
            vec![trig("t"), n("a"), n("b")],
            vec![e("t", "a"), e("a", "b")],
        );
        let runner = |node: &Node, _: &RunContext| {
            if node.id.0 == "a" {
                Err("boom".to_string())
            } else {
                Ok(Value::Null)
            }
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(
            report.step(&NodeId::new("t")).unwrap().status,
            StepStatus::Succeeded
        );
        let a = report.step(&NodeId::new("a")).unwrap();
        assert_eq!(a.status, StepStatus::Failed);
        assert_eq!(a.error.as_deref(), Some("boom"));
        assert_eq!(
            report.step(&NodeId::new("b")).unwrap().status,
            StepStatus::Skipped
        );
    }

    fn ctrl(id: &str, kind: NodeKind, params: Value) -> Node {
        Node {
            position: None,
            id: NodeId::new(id),
            name: id.into(),
            kind,
            params,
        }
    }
    fn ep(from: &str, to: &str, port: &str) -> Edge {
        Edge {
            from: NodeId::new(from),
            to: NodeId::new(to),
            from_port: Some(port.into()),
        }
    }
    fn status(report: &RunReport, id: &str) -> StepStatus {
        report.step(&NodeId::new(id)).unwrap().status
    }

    #[test]
    fn if_takes_true_branch_and_skips_false() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "i",
                    NodeKind::If,
                    serde_json::json!({ "condition": "t.ok == true" }),
                ),
                n("yes"),
                n("no"),
            ],
            vec![e("t", "i"), ep("i", "yes", "true"), ep("i", "no", "false")],
        );
        let runner = |node: &Node, _: &RunContext| {
            assert_ne!(node.id.0, "i", "le runner ne doit pas voir le node `if`");
            Ok(serde_json::json!({ "ok": true }))
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(status(&report, "i"), StepStatus::Succeeded);
        assert_eq!(
            report.step(&NodeId::new("i")).unwrap().output,
            Some(Value::Bool(true))
        );
        assert_eq!(status(&report, "yes"), StepStatus::Succeeded);
        assert_eq!(status(&report, "no"), StepStatus::Skipped);
    }

    #[test]
    fn switch_selects_matching_branch_then_default() {
        let nodes = || {
            vec![
                trig("t"),
                ctrl(
                    "s",
                    NodeKind::Switch,
                    serde_json::json!({
                        "selector": "t.kind",
                        "cases": [
                            { "value": "a", "port": "pa" },
                            { "value": "b", "port": "pb" }
                        ],
                        "default": "pd"
                    }),
                ),
                n("na"),
                n("nb"),
                n("nd"),
            ]
        };
        let edges = || {
            vec![
                e("t", "s"),
                ep("s", "na", "pa"),
                ep("s", "nb", "pb"),
                ep("s", "nd", "pd"),
            ]
        };

        // Cas correspondant : branche `pb`.
        let d = dag(nodes(), edges());
        let r = run_sequential(&d, &|_: &Node, _: &RunContext| {
            Ok(serde_json::json!({ "kind": "b" }))
        });
        assert_eq!(r.status, RunStatus::Succeeded);
        assert_eq!(status(&r, "nb"), StepStatus::Succeeded);
        assert_eq!(status(&r, "na"), StepStatus::Skipped);
        assert_eq!(status(&r, "nd"), StepStatus::Skipped);

        // Aucun cas → branche par défaut `pd`.
        let d = dag(nodes(), edges());
        let r = run_sequential(&d, &|_: &Node, _: &RunContext| {
            Ok(serde_json::json!({ "kind": "z" }))
        });
        assert_eq!(status(&r, "nd"), StepStatus::Succeeded);
        assert_eq!(status(&r, "na"), StepStatus::Skipped);
        assert_eq!(status(&r, "nb"), StepStatus::Skipped);
    }

    #[test]
    fn if_with_invalid_params_fails_the_run() {
        let d = dag(
            vec![
                trig("t"),
                ctrl("i", NodeKind::If, Value::Null), // pas de `condition`
                n("yes"),
                n("no"),
            ],
            vec![e("t", "i"), ep("i", "yes", "true"), ep("i", "no", "false")],
        );
        let report = run_sequential(&d, &|_: &Node, _: &RunContext| Ok(Value::Null));

        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(status(&report, "i"), StepStatus::Failed);
        assert_eq!(status(&report, "yes"), StepStatus::Skipped);
        assert_eq!(status(&report, "no"), StepStatus::Skipped);
    }

    fn output(report: &RunReport, id: &str) -> Value {
        report
            .step(&NodeId::new(id))
            .unwrap()
            .output
            .clone()
            .unwrap()
    }

    #[test]
    fn loop_iterates_collection_and_aggregates_outputs() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "L",
                    NodeKind::Loop,
                    serde_json::json!({ "items": "t.list" }),
                ),
                n("sink"),
            ],
            vec![e("t", "L"), e("L", "sink")],
        );
        let runner = |node: &Node, ctx: &RunContext| match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "list": [1, 2, 3] })),
            "L" => {
                // Le corps lit l'élément courant exposé par le moteur.
                let item = ctx.output(&NodeId::new("L::item")).expect("item").clone();
                let v = item.as_i64().unwrap();
                Ok(Value::from(v * 2))
            }
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(output(&report, "L"), serde_json::json!([2, 4, 6]));
        assert_eq!(status(&report, "sink"), StepStatus::Succeeded);
        // Les clés synthétiques de boucle ne fuitent pas.
        assert!(report.step(&NodeId::new("L::item")).is_none());
    }

    #[test]
    fn loop_over_empty_collection_runs_successors_once() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "L",
                    NodeKind::Loop,
                    serde_json::json!({ "items": "t.list" }),
                ),
                n("sink"),
            ],
            vec![e("t", "L"), e("L", "sink")],
        );
        let runner = |node: &Node, _: &RunContext| match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "list": [] })),
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(output(&report, "L"), serde_json::json!([]));
        assert_eq!(status(&report, "sink"), StepStatus::Succeeded);
    }

    #[test]
    fn loop_rejects_non_array_items() {
        let d = dag(
            vec![
                trig("t"),
                ctrl("L", NodeKind::Loop, serde_json::json!({ "items": "t.x" })),
            ],
            vec![e("t", "L")],
        );
        let runner = |node: &Node, _: &RunContext| match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "x": 42 })),
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);
        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(status(&report, "L"), StepStatus::Failed);
    }

    #[test]
    fn while_runs_until_condition_false_and_aggregates() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "W",
                    NodeKind::While,
                    serde_json::json!({ "condition": "W::index < 3" }),
                ),
            ],
            vec![e("t", "W")],
        );
        let runner = |node: &Node, ctx: &RunContext| match node.id.0.as_str() {
            "W" => Ok(ctx.output(&NodeId::new("W::index")).unwrap().clone()),
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(output(&report, "W"), serde_json::json!([0, 1, 2]));
    }

    #[test]
    fn while_guard_trips_on_infinite_loop() {
        let d = dag(
            vec![
                trig("t"),
                ctrl(
                    "W",
                    NodeKind::While,
                    serde_json::json!({ "condition": "true", "max_iterations": 5 }),
                ),
            ],
            vec![e("t", "W")],
        );
        let report = run_sequential(&d, &|_: &Node, _: &RunContext| Ok(Value::Null));

        assert_eq!(report.status, RunStatus::Failed);
        let w = report.step(&NodeId::new("W")).unwrap();
        assert_eq!(w.status, StepStatus::Failed);
        assert!(w.error.as_deref().unwrap().contains("max_iterations"));
    }

    // --- F2.5 : exécution parallèle -------------------------------------
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Runner qui mesure la concurrence maximale observée sur les nodes
    /// dont l'id commence par `b` (les branches), via une courte pause.
    struct ConcurrencyProbe {
        current: AtomicUsize,
        max: AtomicUsize,
    }
    impl NodeRunner for ConcurrencyProbe {
        fn run(&self, node: &Node, _: &RunContext) -> Result<Value, String> {
            if node.id.0.starts_with('b') {
                let c = self.current.fetch_add(1, Ordering::SeqCst) + 1;
                self.max.fetch_max(c, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(25));
                self.current.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(Value::String(node.id.0.clone()))
        }
    }

    #[test]
    fn fan_out_runs_independent_branches_concurrently() {
        // Diamant : t -> {a? non} ; ici 4 branches indépendantes b1..b4.
        let d = dag(
            vec![trig("t"), n("b1"), n("b2"), n("b3"), n("b4")],
            vec![e("t", "b1"), e("t", "b2"), e("t", "b3"), e("t", "b4")],
        );
        let probe = ConcurrencyProbe {
            current: AtomicUsize::new(0),
            max: AtomicUsize::new(0),
        };
        let report = run_parallel(&d, &probe, 4);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert!(report
            .steps
            .iter()
            .all(|s| s.status == StepStatus::Succeeded));
        // Les 4 branches doivent s'être chevauchées (fan-out réel).
        assert_eq!(probe.max.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn concurrency_limit_is_respected() {
        let d = dag(
            vec![trig("t"), n("b1"), n("b2"), n("b3"), n("b4")],
            vec![e("t", "b1"), e("t", "b2"), e("t", "b3"), e("t", "b4")],
        );
        let probe = ConcurrencyProbe {
            current: AtomicUsize::new(0),
            max: AtomicUsize::new(0),
        };
        let report = run_parallel(&d, &probe, 2);

        assert_eq!(report.status, RunStatus::Succeeded);
        // Jamais plus de 2 branches simultanées (F2.5.c).
        assert_eq!(probe.max.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fan_in_waits_for_all_active_predecessors() {
        // t -> a, t -> b, a -> d, b -> d : d ne part qu'après a ET b.
        let d = dag(
            vec![trig("t"), n("a"), n("b"), n("d")],
            vec![e("t", "a"), e("t", "b"), e("a", "d"), e("b", "d")],
        );
        let runner = |node: &Node, ctx: &RunContext| match node.id.0.as_str() {
            "d" => {
                assert!(ctx.output(&NodeId::new("a")).is_some(), "a manquant");
                assert!(ctx.output(&NodeId::new("b")).is_some(), "b manquant");
                Ok(Value::String("joined".into()))
            }
            other => Ok(Value::String(other.into())),
        };
        let report = run_parallel(&d, &runner, 4);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(output(&report, "d"), serde_json::json!("joined"));
    }

    #[test]
    fn parallel_matches_sequential_branch_semantics() {
        let build = || {
            dag(
                vec![
                    trig("t"),
                    ctrl(
                        "i",
                        NodeKind::If,
                        serde_json::json!({ "condition": "t.ok == true" }),
                    ),
                    n("yes"),
                    n("no"),
                ],
                vec![e("t", "i"), ep("i", "yes", "true"), ep("i", "no", "false")],
            )
        };
        let runner = |node: &Node, _: &RunContext| match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "ok": true })),
            _ => Ok(Value::Null),
        };
        let seq = run_sequential(&build(), &runner);
        let par = run_parallel(&build(), &runner, 3);

        for id in ["t", "i", "yes", "no"] {
            assert_eq!(status(&seq, id), status(&par, id), "divergence sur {id}");
        }
        assert_eq!(par.status, RunStatus::Succeeded);
        assert_eq!(status(&par, "no"), StepStatus::Skipped);
    }

    #[test]
    fn parallel_failure_aborts_and_skips_join() {
        let d = dag(
            vec![trig("t"), n("a"), n("b"), n("d")],
            vec![e("t", "a"), e("t", "b"), e("a", "d"), e("b", "d")],
        );
        let runner = |node: &Node, _: &RunContext| {
            if node.id.0 == "a" {
                Err("boom".to_string())
            } else {
                Ok(Value::Null)
            }
        };
        let report = run_parallel(&d, &runner, 2);

        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(status(&report, "a"), StepStatus::Failed);
        // Le node de jointure ne s'exécute pas après l'échec.
        assert_eq!(status(&report, "d"), StepStatus::Skipped);
    }

    #[test]
    fn runner_receives_resolved_params() {
        let action = Node {
            position: None,
            id: NodeId::new("a"),
            name: "a".into(),
            kind: NodeKind::Action {
                action_type: "http".into(),
            },
            params: serde_json::json!({
                "url": "https://api/{{ t.host }}",
                "n": "{{ t.count }}"
            }),
        };
        let d = dag(vec![trig("t"), action], vec![e("t", "a")]);
        let runner = |node: &Node, _: &RunContext| match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "host": "example", "count": 5 })),
            "a" => {
                // Les `params` doivent arriver résolus (F2.6).
                assert_eq!(node.params["url"], serde_json::json!("https://api/example"));
                assert_eq!(node.params["n"], serde_json::json!(5)); // type préservé
                Ok(Value::Null)
            }
            _ => Ok(Value::Null),
        };
        let report = run_sequential(&d, &runner);
        assert_eq!(report.status, RunStatus::Succeeded);
    }

    // --- F2.7 : gestion d'erreurs ---------------------------------------
    fn act_p(id: &str, params: Value) -> Node {
        Node {
            position: None,
            id: NodeId::new(id),
            name: id.into(),
            kind: NodeKind::Action {
                action_type: "x".into(),
            },
            params,
        }
    }

    /// Runner qui échoue `fails_before_success` fois sur le node `a`.
    struct FlakyRunner {
        fails: usize,
        calls: AtomicUsize,
    }
    impl NodeRunner for FlakyRunner {
        fn run(&self, node: &Node, _: &RunContext) -> Result<Value, String> {
            if node.id.0 == "a" {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n < self.fails {
                    return Err(format!("essai {n} KO"));
                }
            }
            Ok(Value::Null)
        }
    }

    #[test]
    fn retry_succeeds_after_transient_failures() {
        let d = dag(
            vec![
                trig("t"),
                act_p("a", serde_json::json!({ "_error": { "max_attempts": 3 } })),
            ],
            vec![e("t", "a")],
        );
        let runner = FlakyRunner {
            fails: 2,
            calls: AtomicUsize::new(0),
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(status(&report, "a"), StepStatus::Succeeded);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3); // 2 KO + 1 OK
    }

    #[test]
    fn retry_exhausted_then_unhandled_failure_aborts() {
        let d = dag(
            vec![
                trig("t"),
                act_p("a", serde_json::json!({ "_error": { "max_attempts": 2 } })),
                n("b"),
            ],
            vec![e("t", "a"), e("a", "b")],
        );
        let runner = FlakyRunner {
            fails: 99,
            calls: AtomicUsize::new(0),
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Failed);
        assert_eq!(status(&report, "a"), StepStatus::Failed);
        assert_eq!(status(&report, "b"), StepStatus::Skipped); // propagation
        assert_eq!(runner.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn timeout_marks_node_failed() {
        let d = dag(
            vec![
                trig("t"),
                act_p("a", serde_json::json!({ "_error": { "timeout_ms": 20 } })),
            ],
            vec![e("t", "a")],
        );
        let runner = |node: &Node, _: &RunContext| {
            if node.id.0 == "a" {
                std::thread::sleep(Duration::from_millis(80));
            }
            Ok(Value::Null)
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Failed);
        let a = report.step(&NodeId::new("a")).unwrap();
        assert_eq!(a.status, StepStatus::Failed);
        assert!(a.error.as_deref().unwrap().contains("timeout"));
    }

    #[test]
    fn fallback_routes_error_branch_and_run_succeeds() {
        let d = dag(
            vec![
                trig("t"),
                act_p(
                    "a",
                    serde_json::json!({ "_error": { "on_error": "fallback" } }),
                ),
                n("ok"),
                n("h"),
            ],
            vec![e("t", "a"), e("a", "ok"), ep("a", "h", "error")],
        );
        let runner = |node: &Node, _: &RunContext| {
            if node.id.0 == "a" {
                Err("boom".to_string())
            } else {
                Ok(Value::Null)
            }
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded); // échec géré
        assert_eq!(status(&report, "a"), StepStatus::Failed);
        assert_eq!(status(&report, "h"), StepStatus::Succeeded); // handler
        assert_eq!(status(&report, "ok"), StepStatus::Skipped); // pas la sortie défaut
    }

    #[test]
    fn continue_tolerates_error_and_run_succeeds() {
        let d = dag(
            vec![
                trig("t"),
                act_p(
                    "a",
                    serde_json::json!({ "_error": { "on_error": "continue" } }),
                ),
                n("next"),
            ],
            vec![e("t", "a"), e("a", "next")],
        );
        let runner = |node: &Node, _: &RunContext| {
            if node.id.0 == "a" {
                Err("soft".to_string())
            } else {
                Ok(Value::Null)
            }
        };
        let report = run_sequential(&d, &runner);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(status(&report, "a"), StepStatus::Failed);
        assert_eq!(status(&report, "next"), StepStatus::Succeeded);
    }

    #[test]
    fn parallel_honors_fallback_policy() {
        let d = dag(
            vec![
                trig("t"),
                act_p(
                    "a",
                    serde_json::json!({ "_error": { "on_error": "fallback" } }),
                ),
                n("ok"),
                n("h"),
            ],
            vec![e("t", "a"), e("a", "ok"), ep("a", "h", "error")],
        );
        let runner = |node: &Node, _: &RunContext| {
            if node.id.0 == "a" {
                Err("boom".to_string())
            } else {
                Ok(Value::Null)
            }
        };
        let report = run_parallel(&d, &runner, 4);

        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(status(&report, "h"), StepStatus::Succeeded);
        assert_eq!(status(&report, "ok"), StepStatus::Skipped);
    }

    // --- #239 : reprise depuis un curseur -------------------------------

    /// Observateur qui interrompt avant le `stop_at`-ième nœud exécuté et
    /// compte les nœuds vus.
    struct StopAt {
        stop_at: usize,
        before: AtomicUsize,
        after: AtomicUsize,
    }

    impl StopAt {
        fn new(stop_at: usize) -> Self {
            Self {
                stop_at,
                before: AtomicUsize::new(0),
                after: AtomicUsize::new(0),
            }
        }
    }

    impl RunObserver for StopAt {
        fn before_node(&self, _: &NodeId, _: &Cursor) -> Control {
            if self.before.fetch_add(1, Ordering::SeqCst) == self.stop_at {
                Control::Stop
            } else {
                Control::Continue
            }
        }
        fn after_node(&self, _: &Cursor) {
            self.after.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Graphe qui exerce branches, échec géré, boucle et templating.
    fn rich_dag() -> Dag {
        dag(
            vec![
                trig("t"),
                ctrl(
                    "i",
                    NodeKind::If,
                    serde_json::json!({ "condition": "t.ok == true" }),
                ),
                act_p(
                    "a",
                    serde_json::json!({ "_error": { "on_error": "fallback" }, "v": "{{ t.n }}" }),
                ),
                n("no"),
                n("h"),
                ctrl(
                    "L",
                    NodeKind::Loop,
                    serde_json::json!({ "items": "t.list" }),
                ),
                n("sink"),
            ],
            vec![
                e("t", "i"),
                ep("i", "a", "true"),
                ep("i", "no", "false"),
                ep("a", "h", "error"),
                e("h", "L"),
                e("L", "sink"),
            ],
        )
    }

    fn rich_runner(node: &Node, ctx: &RunContext) -> Result<Value, String> {
        match node.id.0.as_str() {
            "t" => Ok(serde_json::json!({ "ok": true, "n": 7, "list": [1, 2] })),
            "a" => Err(format!("refus de {}", node.params["v"])),
            "L" => {
                let item = ctx.output(&NodeId::new("L::item")).expect("item");
                Ok(Value::from(item.as_i64().unwrap() * 10))
            }
            // Lit une sortie amont : prouve que le contexte est restauré.
            "sink" => Ok(ctx.output(&NodeId::new("L")).cloned().unwrap_or_default()),
            // Sortie `null` : doit survivre au passage par JSON.
            "h" => Ok(Value::Null),
            _ => Ok(serde_json::json!({ "at": node.id.0 })),
        }
    }

    #[test]
    fn resuming_at_any_node_reproduces_the_uninterrupted_report() {
        let d = rich_dag();
        let reference = run_sequential(&d, &rich_runner);
        assert_eq!(reference.status, RunStatus::Succeeded);
        assert_eq!(output(&reference, "sink"), serde_json::json!([10, 20]));
        let executed = reference
            .steps
            .iter()
            .filter(|s| s.status != StepStatus::Skipped)
            .count();

        for stop_at in 0..executed {
            let first = StopAt::new(stop_at);
            let interrupted =
                resume_sequential(&d, &rich_runner, Cursor::start(&d), &first).unwrap();
            let Execution::Interrupted { cursor, next } = interrupted else {
                panic!("interruption attendue avant le nœud n°{stop_at}");
            };
            assert_eq!(first.after.load(Ordering::SeqCst), stop_at);
            assert!(cursor.steps().iter().all(|s| s.node_id != next));
            assert_eq!(cursor.done(), cursor.steps().len());

            // Le curseur passe par la base (JSON) avant la reprise.
            let json = serde_json::to_value(&cursor).unwrap();
            let restored: Cursor = serde_json::from_value(json).unwrap();
            let second = StopAt::new(usize::MAX);
            let resumed = resume_sequential(&d, &rich_runner, restored, &second).unwrap();
            assert_eq!(
                resumed,
                Execution::Finished(reference.clone()),
                "reprise avant le nœud n°{stop_at} ({next:?})"
            );
            // Rien n'est rejoué : chaque nœud exécuté l'a été une seule fois.
            assert_eq!(
                first.after.load(Ordering::SeqCst) + second.after.load(Ordering::SeqCst),
                executed
            );
        }
    }

    #[test]
    fn a_resumed_failure_still_skips_the_rest() {
        let d = dag(
            vec![trig("t"), n("a"), n("b")],
            vec![e("t", "a"), e("a", "b")],
        );
        let runner = |node: &Node, _: &RunContext| {
            if node.id.0 == "a" {
                Err("boom".to_string())
            } else {
                Ok(Value::Null)
            }
        };
        // Interrompu juste après l'échec de `a` (avant `b`, jamais exécuté).
        let obs = StopAt::new(usize::MAX);
        let Execution::Finished(full) =
            resume_sequential(&d, &runner, Cursor::start(&d), &obs).unwrap()
        else {
            panic!("terme attendu");
        };
        assert_eq!(obs.after.load(Ordering::SeqCst), 2, "t et a seulement");
        assert_eq!(full, run_sequential(&d, &runner));
        assert_eq!(status(&full, "b"), StepStatus::Skipped);
    }

    #[test]
    fn cursor_exposes_successful_outputs_only() {
        let d = rich_dag();
        let Execution::Interrupted { cursor, .. } =
            resume_sequential(&d, &rich_runner, Cursor::start(&d), &StopAt::new(4)).unwrap()
        else {
            panic!("interruption attendue");
        };
        // t, i, a (échec géré), h traités ; seuls les succès sont exposés.
        let outputs = cursor.outputs();
        assert!(outputs.contains_key("t"));
        assert_eq!(outputs["h"], Value::Null, "sortie `null` gardée");
        assert!(!outputs.contains_key("a"), "échec géré non exposé");
        assert!(!outputs.contains_key("no"), "branche non prise");
    }

    #[test]
    fn a_cursor_from_another_graph_is_refused() {
        let d = rich_dag();
        let Execution::Interrupted { cursor, .. } =
            resume_sequential(&d, &rich_runner, Cursor::start(&d), &StopAt::new(2)).unwrap()
        else {
            panic!("interruption attendue");
        };
        assert!(cursor.fits(&d));
        let changed = dag(vec![trig("t"), n("a")], vec![e("t", "a")]);
        assert!(!cursor.fits(&changed));
        let err = resume_sequential(&changed, &rich_runner, cursor, &StopAt::new(usize::MAX))
            .unwrap_err();
        assert!(err.contains("workflow modifié"), "{err}");
    }

    #[test]
    fn fingerprint_ignores_layout_but_not_parameters() {
        let base = vec![
            trig("t"),
            act_p("a", serde_json::json!({ "url": "https://x" })),
        ];
        let fp = dag(base.clone(), vec![e("t", "a")]).fingerprint();
        // Déplacer ou renommer un node dans l'éditeur ne change rien.
        let mut moved = base.clone();
        moved[1].position = Some(autome_core::NodePosition { x: 3.0, y: 4.0 });
        moved[1].name = "Appel".into();
        assert_eq!(dag(moved, vec![e("t", "a")]).fingerprint(), fp);
        // Un paramètre, un type ou une arête, si.
        let mut param = base.clone();
        param[1].params = serde_json::json!({ "url": "https://y" });
        assert_ne!(dag(param, vec![e("t", "a")]).fingerprint(), fp);
        let mut kind = base.clone();
        kind[1].kind = NodeKind::Action {
            action_type: "slack.post_message".into(),
        };
        assert_ne!(dag(kind, vec![e("t", "a")]).fingerprint(), fp);
        let three = vec![base[0].clone(), base[1].clone(), n("b")];
        assert_ne!(
            dag(three.clone(), vec![e("t", "a"), e("a", "b")]).fingerprint(),
            dag(three, vec![e("t", "a"), e("t", "b")]).fingerprint()
        );
    }

    #[test]
    fn only_side_effect_free_or_retryable_nodes_are_rerunnable() {
        assert!(rerunnable(&trig("t")));
        assert!(rerunnable(&ctrl("i", NodeKind::If, Value::Null)));
        assert!(rerunnable(&ctrl("s", NodeKind::Switch, Value::Null)));
        assert!(!rerunnable(&ctrl("l", NodeKind::Loop, Value::Null)));
        assert!(!rerunnable(&ctrl("w", NodeKind::While, Value::Null)));
        assert!(!rerunnable(&n("a")));
        assert!(!rerunnable(&act_p(
            "a",
            serde_json::json!({ "_error": { "max_attempts": 1, "on_error": "continue" } })
        )));
        assert!(rerunnable(&act_p(
            "a",
            serde_json::json!({ "_error": { "max_attempts": 3 } })
        )));
    }
}

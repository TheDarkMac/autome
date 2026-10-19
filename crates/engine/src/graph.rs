//! Représentation exécutable et validée d'un workflow (F2.1).
//!
//! Construite depuis le modèle agnostique `autome_core::WorkflowGraph`
//! (F1.4) via [`Dag::from_workflow`], qui **valide** :
//! - unicité des identifiants de node (F2.1.a) ;
//! - intégrité des arêtes (références de nodes existants) ;
//! - cohérence des ports de sortie par type de node (F2.1.d) ;
//! - existence d'un **unique** node racine (F2.1.d) ;
//! - **absence de cycle** (F2.1.c).
//!
//! La désérialisation JSON → modèle est assurée par `core` (serde) ; ce
//! module consomme le modèle déjà typé (F2.1.b).
//!
//! **Hors périmètre F2.1** : le tri topologique d'exécution relève de
//! **F2.2.a** ; ici on ne fait que construire et valider la structure.

use std::collections::{HashMap, HashSet};

use autome_core::{Edge, Node, NodeId, NodeKind, WorkflowGraph};
use thiserror::Error;

/// Erreur de construction/validation d'un [`Dag`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    /// Le workflow ne contient aucun node.
    #[error("graphe vide : aucun node")]
    Empty,

    /// Deux nodes partagent le même identifiant.
    #[error("identifiant de node dupliqué : `{0}`")]
    DuplicateNode(String),

    /// Une arête référence un node inexistant.
    #[error("arête vers un node inconnu : `{0}`")]
    UnknownNode(String),

    /// Port de sortie incohérent avec le type du node source.
    #[error("port invalide sur le node `{node}` : {detail}")]
    InvalidPort {
        /// Node source fautif.
        node: String,
        /// Explication de l'incohérence.
        detail: String,
    },

    /// Aucun node racine (tous ont au moins une arête entrante).
    #[error("aucun node racine (graphe sans point d'entrée)")]
    NoRoot,

    /// Plusieurs nodes racine ; un workflow a un point d'entrée unique.
    #[error("plusieurs nodes racine : {0:?}")]
    MultipleRoots(Vec<String>),

    /// Le graphe contient un cycle (un node impliqué est donné).
    #[error("cycle détecté (impliquant le node `{0}`)")]
    Cycle(String),
}

/// Graphe orienté acyclique validé, prêt pour l'exécution (F2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Dag {
    nodes: HashMap<NodeId, Node>,
    /// Arêtes sortantes, indexées par node source.
    out_edges: HashMap<NodeId, Vec<Edge>>,
    /// Ordre d'insertion des nodes (modèle workflow) : sert de tie-break
    /// déterministe au tri topologique (F2.2.a).
    order: Vec<NodeId>,
    root: NodeId,
}

impl Dag {
    /// Construit et valide un DAG à partir du modèle workflow.
    pub fn from_workflow(wf: &WorkflowGraph) -> Result<Self, GraphError> {
        if wf.nodes.is_empty() {
            return Err(GraphError::Empty);
        }

        // Index des nodes + détection de doublons (F2.1.a).
        let mut nodes: HashMap<NodeId, Node> = HashMap::with_capacity(wf.nodes.len());
        for n in &wf.nodes {
            if nodes.insert(n.id.clone(), n.clone()).is_some() {
                return Err(GraphError::DuplicateNode(n.id.0.clone()));
            }
        }

        // Intégrité des arêtes + adjacence + degrés entrants.
        let mut out_edges: HashMap<NodeId, Vec<Edge>> = HashMap::new();
        let mut in_degree: HashMap<NodeId, usize> =
            nodes.keys().map(|id| (id.clone(), 0usize)).collect();
        for e in &wf.edges {
            if !nodes.contains_key(&e.from) {
                return Err(GraphError::UnknownNode(e.from.0.clone()));
            }
            if !nodes.contains_key(&e.to) {
                return Err(GraphError::UnknownNode(e.to.0.clone()));
            }
            *in_degree.get_mut(&e.to).expect("node indexé") += 1;
            out_edges.entry(e.from.clone()).or_default().push(e.clone());
        }

        // Cohérence des ports par type de node source (F2.1.d).
        for (from, edges) in &out_edges {
            let kind = &nodes[from].kind;
            for e in edges {
                validate_port(&from.0, kind, e.from_port.as_deref())?;
            }
        }

        // Node racine unique (F2.1.d).
        let mut roots: Vec<&NodeId> = in_degree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| id)
            .collect();
        match roots.len() {
            0 => return Err(GraphError::NoRoot),
            1 => {}
            _ => {
                roots.sort_by(|a, b| a.0.cmp(&b.0));
                return Err(GraphError::MultipleRoots(
                    roots.iter().map(|id| id.0.clone()).collect(),
                ));
            }
        }
        let root = roots[0].clone();

        let order: Vec<NodeId> = wf.nodes.iter().map(|n| n.id.clone()).collect();
        let dag = Self {
            nodes,
            out_edges,
            order,
            root,
        };
        dag.assert_acyclic()?; // F2.1.c
        Ok(dag)
    }

    /// Empreinte du graphe exécutable (#239) : nodes (type, paramètres) et
    /// arêtes dans l'ordre du modèle, sans la disposition de l'éditeur. Un
    /// point de reprise la porte : reprendre un run sur un graphe modifié
    /// depuis son interruption est refusé.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for id in &self.order {
            let mut node = self.nodes[id].clone();
            node.position = None;
            node.name.clear();
            hasher.update(serde_json::to_vec(&node).expect("node sérialisable"));
            hasher.update([0]);
            for e in self.successors(id) {
                hasher.update(serde_json::to_vec(e).expect("arête sérialisable"));
                hasher.update([0]);
            }
            hasher.update([1]);
        }
        hex::encode(hasher.finalize())
    }

    /// Node racine (point d'entrée unique du workflow).
    pub fn root(&self) -> &Node {
        &self.nodes[&self.root]
    }

    /// Retourne le node d'identifiant donné, s'il existe.
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Arêtes sortantes d'un node (vide si aucune).
    pub fn successors(&self, id: &NodeId) -> &[Edge] {
        self.out_edges.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Nombre de nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// `true` si le graphe ne contient aucun node (jamais le cas après
    /// `from_workflow`, qui rejette le graphe vide — utile pour Clippy).
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Ordre topologique d'exécution (F2.2.a) — algorithme de Kahn.
    ///
    /// Déterministe : à degré entrant nul équivalent, les nodes sortent
    /// dans l'ordre du modèle workflow (`self.order`). Le DAG étant validé
    /// acyclique par [`Dag::from_workflow`], tous les nodes sont émis.
    pub fn topological_order(&self) -> Vec<NodeId> {
        let mut indeg: HashMap<NodeId, usize> = self.nodes.keys().map(|k| (k.clone(), 0)).collect();
        for edges in self.out_edges.values() {
            for e in edges {
                *indeg.get_mut(&e.to).expect("node indexé") += 1;
            }
        }

        let mut out: Vec<NodeId> = Vec::with_capacity(self.order.len());
        let mut emitted: HashSet<NodeId> = HashSet::with_capacity(self.order.len());
        loop {
            let mut progressed = false;
            for id in &self.order {
                if emitted.contains(id) || indeg[id] != 0 {
                    continue;
                }
                out.push(id.clone());
                emitted.insert(id.clone());
                progressed = true;
                if let Some(edges) = self.out_edges.get(id) {
                    for e in edges {
                        *indeg.get_mut(&e.to).expect("node indexé") -= 1;
                    }
                }
            }
            if !progressed {
                break;
            }
        }
        out
    }

    /// Détection de cycle par DFS tricolore (blanc/gris/noir).
    fn assert_acyclic(&self) -> Result<(), GraphError> {
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        let mut color: HashMap<&NodeId, Color> =
            self.nodes.keys().map(|id| (id, Color::White)).collect();

        // Pile explicite : (node, index de l'arête sortante en cours).
        for start in self.nodes.keys() {
            if color[start] != Color::White {
                continue;
            }
            let mut stack: Vec<(&NodeId, usize)> = vec![(start, 0)];
            color.insert(start, Color::Gray);
            while let Some((node, idx)) = stack.last().copied() {
                let succ = self.out_edges.get(node).map(Vec::as_slice).unwrap_or(&[]);
                if idx < succ.len() {
                    stack.last_mut().expect("pile non vide").1 += 1;
                    let next = &succ[idx].to;
                    match color[next] {
                        Color::Gray => return Err(GraphError::Cycle(next.0.clone())),
                        Color::White => {
                            color.insert(next, Color::Gray);
                            stack.push((next, 0));
                        }
                        Color::Black => {}
                    }
                } else {
                    color.insert(node, Color::Black);
                    stack.pop();
                }
            }
        }
        Ok(())
    }
}

/// Règles de port de sortie selon le type du node source (F2.1.d).
fn validate_port(node: &str, kind: &NodeKind, port: Option<&str>) -> Result<(), GraphError> {
    let invalid = |detail: String| {
        Err(GraphError::InvalidPort {
            node: node.to_string(),
            detail,
        })
    };
    match kind {
        // Sortie binaire nommée.
        NodeKind::If => match port {
            Some("true") | Some("false") => Ok(()),
            other => invalid(format!(
                "un node `if` n'a que les ports `true`/`false` (reçu : {other:?})"
            )),
        },
        // Branches nommées libres, mais un port explicite est requis.
        NodeKind::Switch => match port {
            Some(p) if !p.is_empty() => Ok(()),
            _ => invalid("un node `switch` exige un port de branche non vide".into()),
        },
        // Sortie par défaut unique, plus le port réservé `error` pour le
        // handler d'erreur / fallback (F2.7.c).
        NodeKind::Trigger { .. } | NodeKind::Action { .. } | NodeKind::Loop | NodeKind::While => {
            match port {
                None | Some("error") => Ok(()),
                Some(p) => invalid(format!(
                    "ce type de node n'accepte que la sortie par défaut ou `error` (port `{p}` inattendu)"
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            position: None,
            id: NodeId::new(id),
            name: id.to_string(),
            kind,
            params: serde_json::Value::Null,
        }
    }
    fn trig(id: &str) -> Node {
        node(
            id,
            NodeKind::Trigger {
                trigger_type: "webhook".into(),
            },
        )
    }
    fn act(id: &str) -> Node {
        node(
            id,
            NodeKind::Action {
                action_type: "http".into(),
            },
        )
    }
    fn edge(from: &str, to: &str, port: Option<&str>) -> Edge {
        Edge {
            from: NodeId::new(from),
            to: NodeId::new(to),
            from_port: port.map(str::to_string),
        }
    }
    fn wf(nodes: Vec<Node>, edges: Vec<Edge>) -> WorkflowGraph {
        WorkflowGraph {
            id: autome_core::WorkflowId::new("wf"),
            name: "t".into(),
            nodes,
            edges,
        }
    }

    #[test]
    fn builds_valid_linear_graph() {
        let g = wf(
            vec![trig("t"), act("a"), act("b")],
            vec![edge("t", "a", None), edge("a", "b", None)],
        );
        let dag = Dag::from_workflow(&g).expect("valide");
        assert_eq!(dag.len(), 3);
        assert!(!dag.is_empty());
        assert_eq!(dag.root().id, NodeId::new("t"));
        assert_eq!(dag.successors(&NodeId::new("t")).len(), 1);
        assert_eq!(dag.successors(&NodeId::new("b")).len(), 0);
    }

    #[test]
    fn topological_order_is_deterministic() {
        // Diamant : t -> a, t -> b, a -> d, b -> d.
        let g = wf(
            vec![trig("t"), act("a"), act("b"), act("d")],
            vec![
                edge("t", "a", None),
                edge("t", "b", None),
                edge("a", "d", None),
                edge("b", "d", None),
            ],
        );
        let dag = Dag::from_workflow(&g).expect("valide");
        let order: Vec<String> = dag.topological_order().into_iter().map(|n| n.0).collect();
        // t avant a et b ; a et b avant d ; tie-break = ordre du modèle.
        assert_eq!(order, vec!["t", "a", "b", "d"]);
        // Stable d'un appel à l'autre.
        let order2: Vec<String> = dag.topological_order().into_iter().map(|n| n.0).collect();
        assert_eq!(order, order2);
    }

    #[test]
    fn parses_from_deserialized_json_model() {
        // F2.1.b : le modèle vient de serde (core), on le consomme tel quel.
        let raw = r#"{
            "id":"wf-x","name":"j",
            "nodes":[
              {"id":"t","kind":"trigger","trigger_type":"cron"},
              {"id":"x","kind":"action","action_type":"noop"}
            ],
            "edges":[{"from":"t","to":"x"}]
        }"#;
        let model: WorkflowGraph = serde_json::from_str(raw).expect("parse");
        let dag = Dag::from_workflow(&model).expect("valide");
        assert_eq!(dag.root().id, NodeId::new("t"));
    }

    #[test]
    fn rejects_empty_graph() {
        assert_eq!(
            Dag::from_workflow(&wf(vec![], vec![])),
            Err(GraphError::Empty)
        );
    }

    #[test]
    fn rejects_duplicate_node() {
        let g = wf(vec![trig("t"), act("t")], vec![]);
        assert_eq!(
            Dag::from_workflow(&g),
            Err(GraphError::DuplicateNode("t".into()))
        );
    }

    #[test]
    fn rejects_edge_to_unknown_node() {
        let g = wf(vec![trig("t")], vec![edge("t", "ghost", None)]);
        assert_eq!(
            Dag::from_workflow(&g),
            Err(GraphError::UnknownNode("ghost".into()))
        );
    }

    #[test]
    fn rejects_cycle() {
        let g = wf(
            vec![trig("t"), act("a"), act("b")],
            vec![
                edge("t", "a", None),
                edge("a", "b", None),
                edge("b", "a", None),
            ],
        );
        // a a un arc entrant ⇒ pas racine ; t reste racine, cycle a<->b.
        assert!(matches!(Dag::from_workflow(&g), Err(GraphError::Cycle(_))));
    }

    #[test]
    fn rejects_multiple_roots() {
        // t1 et t2 n'ont aucun arc entrant ⇒ deux racines.
        let g = wf(
            vec![trig("t1"), trig("t2"), act("a")],
            vec![edge("t1", "a", None)],
        );
        assert_eq!(
            Dag::from_workflow(&g),
            Err(GraphError::MultipleRoots(vec!["t1".into(), "t2".into()]))
        );
    }

    #[test]
    fn rejects_graph_without_root() {
        // Cycle pur : aucun node sans arc entrant.
        let g = wf(
            vec![act("a"), act("b")],
            vec![edge("a", "b", None), edge("b", "a", None)],
        );
        assert_eq!(Dag::from_workflow(&g), Err(GraphError::NoRoot));
    }

    #[test]
    fn validates_if_ports() {
        let ok = wf(
            vec![trig("t"), node("i", NodeKind::If), act("y"), act("n")],
            vec![
                edge("t", "i", None),
                edge("i", "y", Some("true")),
                edge("i", "n", Some("false")),
            ],
        );
        assert!(Dag::from_workflow(&ok).is_ok());

        // Port `maybe` invalide pour un `if`.
        let ko = wf(
            vec![trig("t"), node("i", NodeKind::If), act("y"), act("n")],
            vec![
                edge("t", "i", None),
                edge("i", "y", Some("maybe")),
                edge("i", "n", Some("false")),
            ],
        );
        assert!(matches!(
            Dag::from_workflow(&ko),
            Err(GraphError::InvalidPort { .. })
        ));
    }

    #[test]
    fn switch_requires_named_port_and_action_forbids_port() {
        let sw = wf(
            vec![trig("t"), node("s", NodeKind::Switch), act("a")],
            vec![edge("t", "s", None), edge("s", "a", None)],
        );
        assert!(matches!(
            Dag::from_workflow(&sw),
            Err(GraphError::InvalidPort { .. })
        ));

        let bad_action = wf(
            vec![trig("t"), act("a"), act("b")],
            vec![edge("t", "a", None), edge("a", "b", Some("x"))],
        );
        assert!(matches!(
            Dag::from_workflow(&bad_action),
            Err(GraphError::InvalidPort { .. })
        ));
    }
}

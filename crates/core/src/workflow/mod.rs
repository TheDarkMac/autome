//! Modèle de données d'un workflow (F1.4).
//!
//! Représentation typée et (dé)sérialisable d'un graphe de workflow :
//! `nodes`, `edges`, paramètres. C'est le format persisté dans
//! `WORKFLOW_VERSIONS.graph` (jsonb — cf. ARCHITECTURE.md §6) et réutilisé
//! par le moteur (F2.1) et l'API (F7.1).
//!
//! **Hors périmètre F1.4** : la *validation* du graphe (détection de
//! cycles, cohérence des ports, node racine unique) relève de **F2.1**.
//! Ici on ne fait que modéliser et offrir des accès en lecture.
//!
//! Le sous-module [`repo`] (F7.1.a) expose un trait `WorkflowRepository`
//! pour persister/lister/charger les workflows — consommé par l'API
//! publique pour le CRUD HTTP.

pub mod repo;

use serde::{Deserialize, Serialize};

use crate::NodeId;

/// Identifiant logique d'un workflow.
///
/// Newtype volontairement minimal ; la correspondance avec la clé primaire
/// `uuid` en base est traitée en F1.5.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkflowId(pub String);

impl WorkflowId {
    /// Construit un identifiant de workflow à partir d'un libellé.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Nature d'un node du graphe.
///
/// Le `snake_case` est figé dans le JSON (clé `kind`) pour stabiliser le
/// schéma. Les sous-types (`trigger_type`, `action_type`) restent des
/// chaînes : le catalogue concret est porté par les domaines 3 et 4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeKind {
    /// Point d'entrée déclencheur (webhook, cron, polling… — F3.*).
    Trigger {
        /// Identifiant du type de trigger (ex. `"webhook"`).
        trigger_type: String,
    },
    /// Action exécutée par un connecteur ou une primitive (F3.*/F4.*).
    Action {
        /// Identifiant du type d'action (ex. `"http_request"`, `"slack.send"`).
        action_type: String,
    },
    /// Branche conditionnelle binaire (F2.3).
    If,
    /// Aiguillage multi-branches (F2.3).
    Switch,
    /// Itération sur une collection (F2.4).
    Loop,
    /// Boucle conditionnelle (F2.4).
    While,
}

/// Un node du graphe : une unité d'exécution paramétrée.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Identifiant unique du node dans le graphe.
    pub id: NodeId,
    /// Libellé lisible (affiché dans l'éditeur visuel).
    #[serde(default)]
    pub name: String,
    /// Nature du node (trigger/action/logique).
    #[serde(flatten)]
    pub kind: NodeKind,
    /// Paramètres libres du node. Le schéma fin par type de node est
    /// validé côté formulaire (F8.4) ; on reste générique ici.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Une arête orientée reliant deux nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// Node source.
    pub from: NodeId,
    /// Node destination.
    pub to: NodeId,
    /// Port de sortie emprunté (ex. `"true"`/`"false"` pour un `If`,
    /// nom de branche pour un `Switch`). `None` = sortie par défaut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_port: Option<String>,
}

/// Le graphe complet d'un workflow : nodes + arêtes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowGraph {
    /// Identifiant du workflow.
    pub id: WorkflowId,
    /// Nom du workflow.
    pub name: String,
    /// Ensemble des nodes.
    #[serde(default)]
    pub nodes: Vec<Node>,
    /// Ensemble des arêtes orientées.
    #[serde(default)]
    pub edges: Vec<Edge>,
}

impl WorkflowGraph {
    /// Retourne le node d'identifiant donné, s'il existe.
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    /// Arêtes entrantes d'un node (celles dont la destination est `id`).
    pub fn incoming<'a>(&'a self, id: &'a NodeId) -> impl Iterator<Item = &'a Edge> {
        self.edges.iter().filter(move |e| &e.to == id)
    }

    /// Arêtes sortantes d'un node (celles dont la source est `id`).
    pub fn outgoing<'a>(&'a self, id: &'a NodeId) -> impl Iterator<Item = &'a Edge> {
        self.edges.iter().filter(move |e| &e.from == id)
    }

    /// Nodes racine : aucun arc entrant. Candidats déclencheurs ; l'unicité
    /// et la validité du point d'entrée sont vérifiées en F2.1.
    pub fn roots(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .iter()
            .filter(|n| self.incoming(&n.id).next().is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WorkflowGraph {
        WorkflowGraph {
            id: WorkflowId::new("wf-1"),
            name: "demo".into(),
            nodes: vec![
                Node {
                    id: NodeId::new("trigger"),
                    name: "Webhook".into(),
                    kind: NodeKind::Trigger {
                        trigger_type: "webhook".into(),
                    },
                    params: serde_json::json!({ "path": "/hook" }),
                },
                Node {
                    id: NodeId::new("act"),
                    name: "HTTP".into(),
                    kind: NodeKind::Action {
                        action_type: "http_request".into(),
                    },
                    params: serde_json::Value::Null,
                },
            ],
            edges: vec![Edge {
                from: NodeId::new("trigger"),
                to: NodeId::new("act"),
                from_port: None,
            }],
        }
    }

    #[test]
    fn graph_roundtrips_through_json() {
        let g = sample();
        let json = serde_json::to_string(&g).expect("serialize");
        let back: WorkflowGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(g, back);
    }

    #[test]
    fn node_kind_uses_stable_tag_in_json() {
        let g = sample();
        let v = serde_json::to_value(&g).expect("to_value");
        assert_eq!(v["nodes"][0]["kind"], "trigger");
        assert_eq!(v["nodes"][0]["trigger_type"], "webhook");
        assert_eq!(v["nodes"][1]["kind"], "action");
    }

    #[test]
    fn deserializes_from_hand_written_json() {
        let raw = r#"{
            "id": "wf-x",
            "name": "manuel",
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger_type": "cron" },
                { "id": "i", "kind": "if" }
            ],
            "edges": [ { "from": "t", "to": "i" } ]
        }"#;
        let g: WorkflowGraph = serde_json::from_str(raw).expect("parse");
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.node(&NodeId::new("i")).unwrap().kind, NodeKind::If);
        // `name` et `params` absents → valeurs par défaut.
        assert_eq!(g.node(&NodeId::new("t")).unwrap().name, "");
        assert_eq!(
            g.node(&NodeId::new("i")).unwrap().params,
            serde_json::Value::Null
        );
    }

    #[test]
    fn lookup_and_adjacency_helpers() {
        let g = sample();
        assert!(g.node(&NodeId::new("trigger")).is_some());
        assert!(g.node(&NodeId::new("absent")).is_none());

        let act = NodeId::new("act");
        assert_eq!(g.incoming(&act).count(), 1);
        assert_eq!(g.outgoing(&act).count(), 0);

        let roots: Vec<_> = g.roots().map(|n| n.id.clone()).collect();
        assert_eq!(roots, vec![NodeId::new("trigger")]);
    }
}

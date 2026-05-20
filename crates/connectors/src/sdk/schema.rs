//! Schémas d'actions (F4.5.c).
//!
//! Description **statique** d'une action et de ses paramètres, destinée à
//! être consommée par :
//! - le frontend (palette de nodes, panneau de configuration — F8.3/F8.4) ;
//! - l'API publique (spec OpenAPI — F7.4).
//!
//! Le schéma est volontairement très simple (pas de validation JSON Schema
//! complète) : nom + type principal + obligatoire/optionnel + libellé court.

use serde::Serialize;

/// Type principal d'un paramètre.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParamKind {
    /// Chaîne libre.
    String,
    /// Entier (non signé ou signé).
    Integer,
    /// Booléen.
    Boolean,
    /// Objet JSON arbitraire (pas de validation interne).
    Object,
    /// Tableau de `inner`.
    Array {
        /// Type des éléments.
        inner: Box<ParamKind>,
    },
    /// Énumération de valeurs admises (strings).
    Enum {
        /// Valeurs permises.
        variants: Vec<&'static str>,
    },
}

/// Spec d'un paramètre.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParamSpec {
    /// Clé JSON dans les `params` du node.
    pub key: &'static str,
    /// Libellé affiché côté UI.
    pub label: &'static str,
    /// Type du paramètre.
    pub kind: ParamKind,
    /// `true` si la clé doit être présente.
    pub required: bool,
    /// Description courte (UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'static str>,
}

impl ParamSpec {
    /// Helper : champ string requis sans description.
    pub const fn req_str(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: ParamKind::String,
            required: true,
            description: None,
        }
    }
    /// Helper : champ string optionnel sans description.
    pub const fn opt_str(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: ParamKind::String,
            required: false,
            description: None,
        }
    }
    /// Helper : champ entier optionnel.
    pub const fn opt_int(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: ParamKind::Integer,
            required: false,
            description: None,
        }
    }
    /// Helper : champ object optionnel.
    pub const fn opt_obj(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: ParamKind::Object,
            required: false,
            description: None,
        }
    }
}

/// Schéma complet d'une action connecteur.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionSchema {
    /// Provider (`"slack"`, `"github"`, …).
    pub provider: &'static str,
    /// Identifiant stable de l'action (égal à `Action::kind()`).
    pub kind: &'static str,
    /// Libellé court (UI).
    pub label: &'static str,
    /// Description longue (UI).
    pub description: &'static str,
    /// Paramètres acceptés par l'action.
    pub params: Vec<ParamSpec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_spec_serializes_to_json() {
        let p = ParamSpec::req_str("channel", "Canal");
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"key\":\"channel\""));
        assert!(s.contains("\"required\":true"));
        assert!(s.contains("\"type\":\"string\""));
    }

    #[test]
    fn enum_kind_serializes_variants() {
        let p = ParamSpec {
            key: "state",
            label: "État",
            kind: ParamKind::Enum {
                variants: vec!["open", "closed"],
            },
            required: false,
            description: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"type\":\"enum\""), "json={s}");
        assert!(s.contains("\"variants\":[\"open\",\"closed\"]"), "json={s}");
    }

    #[test]
    fn array_kind_serializes_inner() {
        let p = ParamSpec {
            key: "labels",
            label: "Labels",
            kind: ParamKind::Array {
                inner: Box::new(ParamKind::String),
            },
            required: false,
            description: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"type\":\"array\""), "json={s}");
        assert!(s.contains("\"inner\":{\"type\":\"string\"}"), "json={s}");
    }

    #[test]
    fn action_schema_round_trips() {
        let schema = ActionSchema {
            provider: "demo",
            kind: "demo.do",
            label: "Faire",
            description: "Faire quelque chose.",
            params: vec![
                ParamSpec::req_str("name", "Nom"),
                ParamSpec::opt_int("count", "Nombre"),
            ],
        };
        let s = serde_json::to_string(&schema).unwrap();
        assert!(s.contains("\"provider\":\"demo\""));
        assert!(s.contains("\"kind\":\"demo.do\""));
        assert!(s.contains("\"required\":true"));
        assert!(s.contains("\"required\":false"));
    }
}

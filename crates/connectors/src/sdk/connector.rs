//! Trait racine d'un connecteur (F4.5.a).
//!
//! Le `Connector` est essentiellement un *registre* : il expose son
//! identifiant de provider et la liste des actions qu'il propose, dans une
//! forme consommable par le frontend (palette de nodes) et l'API publique.
//! Le cycle de vie d'auth concret est porté par les enums `*Auth` de
//! chaque provider, qui implémentent [`crate::sdk::Credentials`].

use autome_core::Result;

use crate::sdk::schema::ActionSchema;

/// Contrat racine d'un connecteur (Slack, Gmail, Notion, GitHub, …).
pub trait Connector {
    /// Identifiant stable du provider (ex. `"slack"`, `"github"`).
    fn provider(&self) -> &'static str;

    /// Liste des actions exposées par le connecteur, sous forme de
    /// schémas statiques (cf. [`ActionSchema`]).
    fn action_schemas(&self) -> Vec<ActionSchema>;

    /// Vérification de configuration/santé optionnelle.
    fn health_check(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::schema::ParamSpec;

    struct Demo;
    impl Connector for Demo {
        fn provider(&self) -> &'static str {
            "demo"
        }
        fn action_schemas(&self) -> Vec<ActionSchema> {
            vec![ActionSchema {
                provider: "demo",
                kind: "demo.do",
                label: "Faire",
                description: "Test.",
                params: vec![ParamSpec::req_str("x", "X")],
            }]
        }
    }

    #[test]
    fn default_health_check_is_ok_and_schemas_listed() {
        let d = Demo;
        assert_eq!(d.provider(), "demo");
        assert_eq!(d.action_schemas().len(), 1);
        assert!(d.health_check().is_ok());
    }
}

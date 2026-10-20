//! Catalogue des connecteurs livrés (#257).
//!
//! Source unique des actions de connecteur proposées à l'interface : l'API
//! le sert tel quel (`GET /connectors/actions`), la palette de nodes et le
//! panneau de configuration du frontend en sont tirés, et un test du worker
//! garantit qu'il sait exécuter chacune de ces actions avec les paramètres
//! que le schéma déclare requis. Un nouveau connecteur s'ajoute ici, en même
//! temps que son dispatch dans le worker.
//!
//! Le connecteur exemple [`crate::template`] n'en fait pas partie : le
//! worker ne l'exécute pas.

use crate::github::GithubConnector;
use crate::gmail::GmailConnector;
use crate::notion::NotionConnector;
use crate::sdk::{ActionSchema, Connector};
use crate::slack::SlackConnector;

/// Connecteurs livrés, dans l'ordre d'affichage.
pub fn connectors() -> Vec<Box<dyn Connector>> {
    vec![
        Box::new(SlackConnector),
        Box::new(GmailConnector),
        Box::new(NotionConnector),
        Box::new(GithubConnector),
    ]
}

/// Toutes les actions des connecteurs livrés, connecteur par connecteur.
pub fn action_schemas() -> Vec<ActionSchema> {
    connectors()
        .iter()
        .flat_map(|c| c.action_schemas())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn lists_the_shipped_connectors_without_the_template() {
        let providers: Vec<&str> = connectors().iter().map(|c| c.provider()).collect();
        assert_eq!(providers, ["slack", "gmail", "notion", "github"]);
    }

    #[test]
    fn every_action_belongs_to_its_connector() {
        for connector in connectors() {
            let provider = connector.provider();
            for schema in connector.action_schemas() {
                assert_eq!(schema.provider, provider, "{}", schema.kind);
                assert!(
                    schema.kind.starts_with(&format!("{provider}.")),
                    "{} doit être préfixé par `{provider}.`",
                    schema.kind
                );
            }
        }
    }

    #[test]
    fn kinds_and_param_keys_are_unique() {
        let schemas = action_schemas();
        assert_eq!(schemas.len(), 15);
        let kinds: HashSet<&str> = schemas.iter().map(|s| s.kind).collect();
        assert_eq!(kinds.len(), schemas.len(), "kind en double");
        for schema in &schemas {
            let keys: HashSet<&str> = schema.params.iter().map(|p| p.key).collect();
            assert_eq!(
                keys.len(),
                schema.params.len(),
                "{} : clé en double",
                schema.kind
            );
            assert!(!schema.label.is_empty() && !schema.description.is_empty());
        }
    }
}

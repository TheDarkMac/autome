//! Abstraction des actions de workflow (F3.5).
//!
//! Une *action* est l'unité de travail exécutée par un node `Action` :
//! elle reçoit des paramètres déjà résolus (mapping F2.6) et produit une
//! sortie JSON. Le contrat est volontairement minimal et **agnostique**
//! (aucune dépendance réseau dans `core`). L'implémentation HTTP générique
//! relève de F3.6 ; les connecteurs tiers du domaine 4.

use serde_json::Value;

use crate::Result;

/// Contrat commun à toutes les actions (extensible).
pub trait Action {
    /// Identifiant stable du type d'action (ex. `"http_request"`).
    fn kind(&self) -> &'static str;

    /// Exécute l'action avec ses `params` (déjà résolus) et renvoie sa
    /// sortie. Une `Err` signale un échec d'exécution (politique d'erreur
    /// gérée par le moteur — F2.7).
    fn execute(&self, params: &Value) -> Result<Value>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    /// Action factice : additionne `params.a` et `params.b`.
    struct Adder;
    impl Action for Adder {
        fn kind(&self) -> &'static str {
            "adder"
        }
        fn execute(&self, params: &Value) -> Result<Value> {
            let a = params
                .get("a")
                .and_then(Value::as_i64)
                .ok_or_else(|| Error::Invalid("`a` manquant".into()))?;
            let b = params
                .get("b")
                .and_then(Value::as_i64)
                .ok_or_else(|| Error::Invalid("`b` manquant".into()))?;
            Ok(serde_json::json!({ "sum": a + b }))
        }
    }

    #[test]
    fn executes_and_reports_kind() {
        let act = Adder;
        assert_eq!(act.kind(), "adder");
        let out = act.execute(&serde_json::json!({ "a": 2, "b": 5 })).unwrap();
        assert_eq!(out, serde_json::json!({ "sum": 7 }));
    }

    #[test]
    fn missing_param_is_an_error() {
        let act = Adder;
        let err = act.execute(&serde_json::json!({ "a": 1 })).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }
}

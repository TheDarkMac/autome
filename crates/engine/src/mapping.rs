//! Passage & mapping de données entre nodes (F2.6).
//!
//! - **F2.6.a** — moteur d'expressions : réutilise [`condition::resolve`]
//!   (littéraux + chemins `node.champ` sur le [`RunContext`]).
//! - **F2.6.b** — templating `{{ expr }}` dans les chaînes : multi-occurrences
//!   interpolées ; si la chaîne est **exactement** un seul `{{ … }}`, le
//!   type JSON résolu est **préservé** (pas de conversion en chaîne).
//! - **F2.6.c** — mapping : [`resolve_params`] résout récursivement les
//!   `params` d'un node (objets/tableaux/chaînes) avant exécution.
//!
//! Les nodes de contrôle (if/switch/loop/while) conservent leurs
//! expressions **brutes** (ils les évaluent eux-mêmes) ; seul le chemin
//! runner (trigger/action) reçoit des `params` résolus (cf. `exec`).

use autome_core::Node;
use serde_json::Value;

use crate::condition;
use crate::exec::RunContext;

/// Évalue une expression (F2.6.a) : littéral ou chemin de contexte.
pub fn eval_expression(expr: &str, ctx: &RunContext) -> Result<Value, String> {
    condition::resolve(expr, ctx)
}

/// Rend une valeur JSON en chaîne pour l'interpolation (`{{ }}` inline).
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(), // tableaux/objets → JSON compact
    }
}

/// Si `s` est exactement un unique `{{ … }}` (après trim), renvoie
/// l'expression interne (type à préserver) ; sinon `None`.
fn sole_expression(s: &str) -> Option<&str> {
    let t = s.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner.trim())
}

/// Templating de chaîne (F2.6.b) : remplace chaque `{{ expr }}` par la
/// valeur résolue rendue en chaîne. Le texte hors balises est conservé.
pub fn render_template(input: &str, ctx: &RunContext) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("`{{{{` non fermé dans `{input}`"))?;
        let expr = after[..end].trim();
        let value = condition::resolve(expr, ctx)?;
        out.push_str(&value_to_string(&value));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Résout récursivement une valeur (F2.6.b/c) :
/// - chaîne « unique expression » → valeur typée (nombre, objet, …) ;
/// - autre chaîne → templating ;
/// - tableau/objet → résolution élément par élément ;
/// - reste → inchangé.
pub fn resolve_value(value: &Value, ctx: &RunContext) -> Result<Value, String> {
    match value {
        Value::String(s) => {
            if let Some(expr) = sole_expression(s) {
                condition::resolve(expr, ctx)
            } else if s.contains("{{") {
                Ok(Value::String(render_template(s, ctx)?))
            } else {
                Ok(Value::String(s.clone()))
            }
        }
        Value::Array(items) => items
            .iter()
            .map(|v| resolve_value(v, ctx))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), resolve_value(v, ctx)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// Mapping des champs d'entrée d'un node (F2.6.c) : renvoie ses `params`
/// résolus contre le contexte courant.
pub fn resolve_params(node: &Node, ctx: &RunContext) -> Result<Value, String> {
    resolve_value(&node.params, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{NodeId, NodeKind};
    use serde_json::json;

    fn ctx() -> RunContext {
        let mut c = RunContext::default();
        c.record_for_test(
            NodeId::new("t"),
            json!({ "name": "Bob", "count": 3, "tags": ["a", "b"] }),
        );
        c
    }

    #[test]
    fn expression_reads_context() {
        let c = ctx();
        assert_eq!(eval_expression("t.name", &c).unwrap(), json!("Bob"));
        assert_eq!(eval_expression("'lit'", &c).unwrap(), json!("lit"));
        assert!(eval_expression("missing.x", &c).is_err());
    }

    #[test]
    fn template_interpolates_multiple_occurrences() {
        let c = ctx();
        assert_eq!(
            render_template("Hi {{ t.name }}, n={{ t.count }}", &c).unwrap(),
            "Hi Bob, n=3"
        );
        assert_eq!(
            render_template("aucune balise", &c).unwrap(),
            "aucune balise"
        );
        assert!(render_template("{{ t.name", &c).is_err());
        assert!(render_template("{{ nope.x }}", &c).is_err());
    }

    #[test]
    fn sole_expression_preserves_type() {
        let c = ctx();
        assert_eq!(
            resolve_value(&json!("{{ t.count }}"), &c).unwrap(),
            json!(3)
        );
        assert_eq!(
            resolve_value(&json!("{{ t.tags }}"), &c).unwrap(),
            json!(["a", "b"])
        );
        // Mélange texte + balise → chaîne.
        assert_eq!(
            resolve_value(&json!("n={{ t.count }}"), &c).unwrap(),
            json!("n=3")
        );
    }

    #[test]
    fn resolves_nested_params() {
        let c = ctx();
        let node = Node {
            id: NodeId::new("a"),
            name: "a".into(),
            kind: NodeKind::Action {
                action_type: "http".into(),
            },
            params: json!({
                "url": "https://api/{{ t.name }}",
                "retries": "{{ t.count }}",
                "headers": { "x": "{{ t.name }}" },
                "list": ["{{ t.count }}", "static"]
            }),
        };
        let r = resolve_params(&node, &c).unwrap();
        assert_eq!(r["url"], json!("https://api/Bob"));
        assert_eq!(r["retries"], json!(3)); // type préservé
        assert_eq!(r["headers"]["x"], json!("Bob"));
        assert_eq!(r["list"], json!([3, "static"]));
    }
}

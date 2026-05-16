//! Évaluation d'expressions de condition (F2.3.c).
//!
//! Langage volontairement compact et déterministe — pas un moteur de
//! templating (ça, c'est F2.6). Une condition est :
//!
//! - une comparaison binaire `‹operande› ‹op› ‹operande›`
//!   (`==`, `!=`, `<`, `<=`, `>`, `>=`) ;
//! - ou un opérande seul, évalué par *truthiness*.
//!
//! Un **opérande** est :
//! - un littéral chaîne entre apostrophes : `'texte'` ;
//! - un nombre (`42`, `3.14`), un booléen (`true`/`false`), `null` ;
//! - un **chemin** `node_id.champ.sous_champ` (ou `[index]` pour un
//!   tableau) résolu sur la sortie d'un node via le [`RunContext`].
//!
//! L'évaluation ne panique jamais : toute incohérence renvoie `Err`.

use serde_json::Value;

use crate::exec::RunContext;

/// Évalue une condition booléenne.
pub fn eval_bool(expr: &str, ctx: &RunContext) -> Result<bool, String> {
    let expr = expr.trim();
    if let Some((lhs, op, rhs)) = split_comparison(expr) {
        let l = resolve(lhs, ctx)?;
        let r = resolve(rhs, ctx)?;
        compare(&l, op, &r)
    } else {
        Ok(truthy(&resolve(expr, ctx)?))
    }
}

/// Résout un opérande (littéral ou chemin de contexte) en valeur JSON.
pub fn resolve(token: &str, ctx: &RunContext) -> Result<Value, String> {
    let t = token.trim();
    if t.is_empty() {
        return Err("opérande vide".to_string());
    }
    // Littéral chaîne : 'xxx'
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return Ok(Value::String(t[1..t.len() - 1].to_string()));
    }
    match t {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "null" => return Ok(Value::Null),
        _ => {}
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(Value::from(i));
    }
    if let Ok(f) = t.parse::<f64>() {
        return Ok(Value::from(f));
    }
    // Chemin : premier segment = id de node, reste = descente JSON.
    let mut segments = t.split('.');
    let node = segments.next().expect("au moins un segment");
    let mut cur = ctx
        .output(&autome_core::NodeId::new(node))
        .ok_or_else(|| format!("sortie du node `{node}` introuvable"))?;
    for seg in segments {
        cur = descend(cur, seg).ok_or_else(|| format!("chemin `{t}` : segment `{seg}` absent"))?;
    }
    Ok(cur.clone())
}

fn descend<'a>(value: &'a Value, seg: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map.get(seg),
        Value::Array(arr) => seg.parse::<usize>().ok().and_then(|i| arr.get(i)),
        _ => None,
    }
}

/// Découpe `expr` au premier opérateur de comparaison rencontré.
/// Les opérateurs à deux caractères sont testés avant ceux à un seul.
fn split_comparison(expr: &str) -> Option<(&str, &str, &str)> {
    for op in ["==", "!=", "<=", ">="] {
        if let Some(i) = expr.find(op) {
            return Some((&expr[..i], op, &expr[i + 2..]));
        }
    }
    for op in ["<", ">"] {
        if let Some(i) = expr.find(op) {
            return Some((&expr[..i], op, &expr[i + 1..]));
        }
    }
    None
}

/// Égalité JSON avec normalisation numérique (`1` == `1.0`).
pub fn json_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if a.is_number() && b.is_number() => x == y,
        _ => a == b,
    }
}

fn compare(l: &Value, op: &str, r: &Value) -> Result<bool, String> {
    match op {
        "==" => Ok(json_eq(l, r)),
        "!=" => Ok(!json_eq(l, r)),
        "<" | "<=" | ">" | ">=" => {
            let ord = if let (Some(x), Some(y)) = (l.as_f64(), r.as_f64()) {
                x.partial_cmp(&y)
            } else if let (Value::String(x), Value::String(y)) = (l, r) {
                Some(x.cmp(y))
            } else {
                return Err(format!("comparaison `{op}` impossible entre {l} et {r}"));
            };
            let ord = ord.ok_or_else(|| "comparaison non ordonnable (NaN ?)".to_string())?;
            Ok(match op {
                "<" => ord.is_lt(),
                "<=" => ord.is_le(),
                ">" => ord.is_gt(),
                _ => ord.is_ge(),
            })
        }
        other => Err(format!("opérateur inconnu : `{other}`")),
    }
}

/// *Truthiness* d'une valeur (opérande seul).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::NodeId;
    use serde_json::json;

    fn ctx_with(id: &str, v: Value) -> RunContext {
        let mut c = RunContext::default();
        c.record_for_test(NodeId::new(id), v);
        c
    }

    #[test]
    fn literals_and_truthiness() {
        let ctx = RunContext::default();
        assert!(eval_bool("true", &ctx).unwrap());
        assert!(!eval_bool("false", &ctx).unwrap());
        assert!(!eval_bool("null", &ctx).unwrap());
        assert!(eval_bool("'x'", &ctx).unwrap());
        assert!(!eval_bool("''", &ctx).unwrap());
        assert!(eval_bool("1", &ctx).unwrap());
        assert!(!eval_bool("0", &ctx).unwrap());
    }

    #[test]
    fn numeric_and_string_comparisons() {
        let ctx = RunContext::default();
        assert!(eval_bool("2 > 1", &ctx).unwrap());
        assert!(eval_bool("1 <= 1", &ctx).unwrap());
        assert!(eval_bool("1 == 1.0", &ctx).unwrap());
        assert!(eval_bool("'a' != 'b'", &ctx).unwrap());
        assert!(eval_bool("'abc' < 'abd'", &ctx).unwrap());
    }

    #[test]
    fn resolves_context_paths() {
        let ctx = ctx_with("t", json!({ "user": { "age": 30 }, "tags": ["x"] }));
        assert!(eval_bool("t.user.age >= 18", &ctx).unwrap());
        assert!(eval_bool("t.user.age < 65", &ctx).unwrap());
        assert!(eval_bool("t.tags.0 == 'x'", &ctx).unwrap());
        assert!(eval_bool("t.user", &ctx).unwrap()); // objet non vide → truthy
    }

    #[test]
    fn errors_are_returned_not_panicked() {
        let ctx = RunContext::default();
        assert!(eval_bool("missing.field == 1", &ctx).is_err());
        assert!(eval_bool("'a' < 1", &ctx).is_err());
        assert!(resolve("", &ctx).is_err());
    }
}

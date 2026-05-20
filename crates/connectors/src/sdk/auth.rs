//! Auth des connecteurs (F4.5.a).
//!
//! Le trait [`Credentials`] représente une stratégie d'authentification :
//! son impl pose les en-têtes nécessaires (Bearer + extras propres au
//! provider : `Notion-Version`, `X-GitHub-Api-Version`, `User-Agent`, …).
//!
//! [`headers_for`] est le helper canonique appelé par chaque connecteur
//! lors de la construction d'une requête : il applique les en-têtes d'auth
//! puis ajoute `Content-Type: application/json; charset=utf-8` quand la
//! requête porte un body.

use std::collections::BTreeMap;

/// Stratégie d'authentification d'un connecteur.
pub trait Credentials {
    /// Ajoute (ou remplace) les en-têtes d'authentification dans `headers`.
    fn apply(&self, headers: &mut BTreeMap<String, String>);
}

/// Construit les en-têtes communs d'une requête connecteur :
/// applique [`Credentials::apply`] puis ajoute `Content-Type` si `has_body`.
pub fn headers_for<C: Credentials>(creds: &C, has_body: bool) -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    creds.apply(&mut h);
    if has_body {
        h.insert(
            "Content-Type".into(),
            "application/json; charset=utf-8".into(),
        );
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyCreds;
    impl Credentials for DummyCreds {
        fn apply(&self, h: &mut BTreeMap<String, String>) {
            h.insert("Authorization".into(), "Bearer dummy".into());
            h.insert("X-Extra".into(), "ok".into());
        }
    }

    #[test]
    fn headers_for_includes_auth_and_content_type_on_body() {
        let h = headers_for(&DummyCreds, true);
        assert_eq!(
            h.get("Authorization").map(String::as_str),
            Some("Bearer dummy")
        );
        assert_eq!(h.get("X-Extra").map(String::as_str), Some("ok"));
        assert_eq!(
            h.get("Content-Type").map(String::as_str),
            Some("application/json; charset=utf-8")
        );
    }

    #[test]
    fn headers_for_skips_content_type_without_body() {
        let h = headers_for(&DummyCreds, false);
        assert_eq!(
            h.get("Authorization").map(String::as_str),
            Some("Bearer dummy")
        );
        assert!(!h.contains_key("Content-Type"));
    }
}

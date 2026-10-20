//! Helpers URL/query (F4.5.b).
//!
//! Volontairement minimaux et sans dépendance tierce : tous les connecteurs
//! les consomment via le SDK. Doublons précédents (slack/gmail/notion/github)
//! supprimés à la livraison de F4.5.

/// Encodage URL minimal (RFC 3986 « unreserved » : `A-Z a-z 0-9 -_.~`).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Sérialise une liste `(clé, valeur)` en query string `k=v&k=v…`, chaque
/// clé et valeur étant [`percent_encode`]-ées. Ordre préservé. Sert aussi
/// les corps `application/x-www-form-urlencoded` (même encodage).
pub fn to_query<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                percent_encode(k.as_ref()),
                percent_encode(v.as_ref())
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_preserves_unreserved() {
        assert_eq!(percent_encode("abc-_.~XYZ012"), "abc-_.~XYZ012");
    }

    #[test]
    fn percent_encode_escapes_reserved() {
        assert_eq!(percent_encode("a/b c"), "a%2Fb%20c");
        assert_eq!(percent_encode("x=y&z"), "x%3Dy%26z");
        assert_eq!(percent_encode("@"), "%40");
        assert_eq!(percent_encode(":"), "%3A");
    }

    #[test]
    fn percent_encode_handles_utf8_bytes() {
        // « é » = 0xC3 0xA9 en UTF-8
        assert_eq!(percent_encode("é"), "%C3%A9");
    }

    #[test]
    fn to_query_keeps_pair_order_and_encodes() {
        let pairs: Vec<(String, String)> = vec![
            ("channel".into(), "C1".into()),
            ("limit".into(), "50".into()),
            ("cursor".into(), "abc=".into()),
        ];
        assert_eq!(to_query(&pairs), "channel=C1&limit=50&cursor=abc%3D");
    }

    #[test]
    fn to_query_handles_empty() {
        assert_eq!(to_query::<&str, &str>(&[]), "");
    }

    #[test]
    fn to_query_accepts_borrowed_pairs() {
        assert_eq!(to_query(&[("a b", "c&d")]), "a%20b=c%26d");
    }
}

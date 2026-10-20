//! Jetons sans état des liens envoyés par email (#204, #231).
//!
//! Un jeton vaut `<user_id>.<expiration>.<mac>` où
//! `mac = HMAC-SHA256(clé, user_id ‖ expiration ‖ valeur liée)`. La clé est
//! dérivée d'un secret partagé par toutes les répliques (`ENCRYPTION_KEY`)
//! sous un **préfixe de domaine** propre à chaque usage : un jeton de
//! réinitialisation n'est jamais accepté comme lien de vérification, et
//! inversement. La valeur liée est l'état du compte que le lien doit
//! retrouver intact (hash du mot de passe, email) : la changer invalide le
//! jeton, sans rien stocker ni purger.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Émission et vérification des jetons d'un usage donné.
#[derive(Clone)]
pub struct LinkTokens {
    pub(crate) key: [u8; 32],
    ttl_seconds: i64,
}

impl std::fmt::Debug for LinkTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinkTokens(<redacted>)")
    }
}

/// Jeton découpé, pas encore vérifié.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLinkToken<'a> {
    /// Compte visé (`users.id`).
    pub user_id: &'a str,
    /// Expiration (secondes Unix).
    pub expires_at: i64,
    mac: Vec<u8>,
}

impl LinkTokens {
    /// Clé dérivée de `secret` sous le préfixe `domain` (ex.
    /// `autome/password-reset/v1`) ; jetons valables `ttl_seconds`.
    pub fn new(domain: &str, secret: &[u8], ttl_seconds: i64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain.as_bytes());
        hasher.update(b"\0");
        hasher.update(secret);
        Self {
            key: hasher.finalize().into(),
            ttl_seconds,
        }
    }

    fn mac(&self, user_id: &str, expires_at: i64, bound: &str) -> Hmac<Sha256> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepte toute longueur");
        // Champs séparés par un octet nul : aucune concaténation ambiguë.
        mac.update(user_id.as_bytes());
        mac.update(b"\0");
        mac.update(expires_at.to_string().as_bytes());
        mac.update(b"\0");
        mac.update(bound.as_bytes());
        mac
    }

    /// Jeton pour le compte `user_id` dans l'état `bound`, valable à partir
    /// de `now`.
    pub fn issue(&self, user_id: &str, bound: &str, now: i64) -> String {
        let expires_at = now.saturating_add(self.ttl_seconds);
        let mac = self.mac(user_id, expires_at, bound).finalize().into_bytes();
        format!("{user_id}.{expires_at}.{}", hex::encode(mac))
    }

    /// Découpe `token` sans le vérifier ; `None` s'il est mal formé.
    pub fn parse(token: &str) -> Option<ParsedLinkToken<'_>> {
        let mut parts = token.split('.');
        let (user_id, expires_at, mac) = (parts.next()?, parts.next()?, parts.next()?);
        // L'identifiant vient d'une saisie non fiable et finit dans l'audit :
        // forme d'un `users.id` (UUID) seulement.
        let id_ok = !user_id.is_empty()
            && user_id.len() <= 64
            && user_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if parts.next().is_some() || !id_ok {
            return None;
        }
        Some(ParsedLinkToken {
            user_id,
            expires_at: expires_at.parse().ok()?,
            mac: unhex(mac)?,
        })
    }

    /// `true` si `token` est authentique, non expiré à `now`, et émis pour
    /// l'état **actuel** `bound` du compte (comparaison à temps constant).
    pub fn verify(&self, token: &ParsedLinkToken<'_>, bound: &str, now: i64) -> bool {
        token.expires_at > now
            && self
                .mac(token.user_id, token.expires_at, bound)
                .verify_slice(&token.mac)
                .is_ok()
    }
}

/// MAC hexadécimal d'un jeton : exactement 32 octets (HMAC-SHA-256).
fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 {
        return None;
    }
    hex::decode(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const NOW: i64 = 1_800_000_000;

    #[test]
    fn domains_separate_keys_and_tokens() {
        let reset = LinkTokens::new("autome/password-reset/v1", b"k", 60);
        let verify = LinkTokens::new("autome/email-verification/v1", b"k", 60);
        assert_ne!(reset.key, verify.key);
        let token = reset.issue(UID, "state", NOW);
        let parsed = LinkTokens::parse(&token).unwrap();
        assert!(reset.verify(&parsed, "state", NOW));
        assert!(!verify.verify(&parsed, "state", NOW), "autre usage");
        assert_eq!(format!("{verify:?}"), "LinkTokens(<redacted>)");
    }

    #[test]
    fn ttl_is_per_usage() {
        let short = LinkTokens::new("d", b"k", 60);
        let parsed_expiry = |t: &LinkTokens| {
            let token = t.issue(UID, "s", NOW);
            LinkTokens::parse(&token).unwrap().expires_at
        };
        assert_eq!(parsed_expiry(&short), NOW + 60);
        assert_eq!(parsed_expiry(&LinkTokens::new("d", b"k", 3600)), NOW + 3600);
    }
}

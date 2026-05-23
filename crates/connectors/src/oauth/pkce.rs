//! Helpers PKCE + state aléatoires (F6.2.a).
//!
//! [`generate_pkce`] : paire `(code_verifier, code_challenge)` au format
//! S256 (RFC 7636). `code_verifier` = 32 octets aléatoires (OsRng) → base64url
//! sans padding (~43 chars, dans la plage 43-128 imposée par la RFC).
//! `code_challenge` = base64url(SHA-256(code_verifier)).
//!
//! [`generate_state`] : 32 octets aléatoires base64url, pour le paramètre
//! `state` CSRF. Caller doit le stocker côté serveur jusqu'au callback.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Paire PKCE — le `verifier` reste côté serveur, le `challenge` part dans
/// l'URL d'autorisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    /// `code_verifier` (RFC 7636 §4.1) : 43-128 caractères URL-safe.
    pub verifier: String,
    /// `code_challenge` (RFC 7636 §4.2) : `base64url(sha256(verifier))`.
    pub challenge: String,
}

/// Génère une paire PKCE S256 fraîche.
pub fn generate_pkce() -> PkcePair {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    PkcePair {
        verifier,
        challenge,
    }
}

/// Génère un `state` CSRF (32 octets aléatoires base64url).
pub fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set des caractères autorisés en base64url-safe sans padding (RFC 4648 §5).
    fn is_url_safe(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '-' || c == '_'
    }

    #[test]
    fn pkce_verifier_uses_url_safe_alphabet() {
        let p = generate_pkce();
        assert!(
            p.verifier.chars().all(is_url_safe),
            "verifier={}",
            p.verifier
        );
        // 32 octets → 43 chars base64url no-padding
        assert_eq!(p.verifier.len(), 43);
    }

    #[test]
    fn pkce_challenge_is_url_safe_and_correct_length() {
        let p = generate_pkce();
        assert!(
            p.challenge.chars().all(is_url_safe),
            "challenge={}",
            p.challenge
        );
        // SHA-256 = 32 octets → 43 chars base64url no-padding
        assert_eq!(p.challenge.len(), 43);
    }

    #[test]
    fn pkce_challenge_is_deterministic_from_verifier() {
        // Le challenge est `base64url(sha256(verifier))` — on peut le recalculer.
        let p = generate_pkce();
        let mut h = Sha256::new();
        h.update(p.verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(h.finalize());
        assert_eq!(p.challenge, expected);
    }

    #[test]
    fn pkce_pairs_are_unique_across_calls() {
        let a = generate_pkce();
        let b = generate_pkce();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn state_is_url_safe_and_unique() {
        let s1 = generate_state();
        let s2 = generate_state();
        assert!(s1.chars().all(is_url_safe));
        assert_eq!(s1.len(), 43);
        assert_ne!(s1, s2);
    }

    /// Vecteur de référence pour SHA-256 : "abc" → ba7816bf… (FIPS-180-4).
    /// Vérifie indirectement que notre encodage du challenge utilise la même
    /// implem SHA-256 que les autres modules (`hmac`, `signature webhook`…).
    #[test]
    fn sha256_underlying_is_consistent_with_fips_vector() {
        let mut h = Sha256::new();
        h.update(b"abc");
        let digest = h.finalize();
        // Bytes attendus de SHA-256("abc")
        assert_eq!(
            hex::encode(digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

//! Chiffrement AES-256-GCM avec format envelope versionné (F6.3.b + F6.3.c).
//!
//! ## Format de l'envelope (texte stockable en colonne `TEXT`)
//!
//! ```text
//! v1:<key_id>:<nonce_b64>:<ciphertext_and_tag_b64>
//! ```
//!
//! - `v1` : version du format (préfixe fixe — autorise une future `v2`
//!   sans casser les enregistrements existants) ;
//! - `<key_id>` : identifiant de la clé dans le [`KeyProvider`]. ASCII
//!   sans `:` ;
//! - `<nonce_b64>` : 12 octets aléatoires (base64-standard) ;
//! - `<ciphertext_and_tag_b64>` : ciphertext concaténé au tag GCM 128
//!   bits (base64-standard).
//!
//! Tout chiffrement consomme un nonce frais via `OsRng` → deux appels
//! `encrypt(plaintext)` produisent deux envelopes différentes.
//!
//! ## Rotation (F6.3.c)
//!
//! - `encrypt` utilise toujours la clé `provider.current()` ;
//! - `decrypt` lit le `key_id` dans l'envelope et résout via
//!   `provider.get(key_id)` — les **anciennes** clés doivent rester
//!   présentes dans le `KeyProvider` tant que des secrets sont chiffrés
//!   avec elles ;
//! - [`EnvelopeCipher::reencrypt`] décode avec l'ancienne clé et ré-encode
//!   avec la courante — utilisable par une routine de re-chiffrement
//!   progressif après ajout d'une nouvelle clé.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rand::RngCore;

use crate::secrets::keys::KeyProvider;
use crate::{Error, Result};

const FORMAT_VERSION: &str = "v1";
const NONCE_LEN: usize = 12;

/// Cipher transparent qui consomme un [`KeyProvider`] et produit des
/// envelopes texte stockables en base.
pub struct EnvelopeCipher<K: KeyProvider> {
    keys: K,
}

impl<K: KeyProvider> EnvelopeCipher<K> {
    /// Construit le cipher autour d'un `KeyProvider`.
    pub fn new(keys: K) -> Self {
        Self { keys }
    }

    /// Accès au provider sous-jacent (utile pour les tests / la rotation).
    pub fn keys(&self) -> &K {
        &self.keys
    }

    /// Chiffre `plaintext` avec la clé courante. Génère un nonce frais
    /// via `OsRng`.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String> {
        let key_id = self.keys.current()?;
        self.encrypt_with(&key_id, plaintext)
    }

    fn encrypt_with(&self, key_id: &str, plaintext: &[u8]) -> Result<String> {
        if key_id.contains(':') {
            return Err(Error::Invalid("key_id contient ':' (interdit)".into()));
        }
        let key_bytes = self.keys.get(key_id)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| Error::Invalid("encrypt : échec AES-GCM".into()))?;

        Ok(format!(
            "{FORMAT_VERSION}:{key_id}:{}:{}",
            BASE64.encode(nonce_bytes),
            BASE64.encode(&ciphertext)
        ))
    }

    /// Déchiffre une envelope, en sélectionnant la clé d'origine via le
    /// `key_id` qu'elle porte.
    pub fn decrypt(&self, envelope: &str) -> Result<Vec<u8>> {
        let (key_id, nonce_b64, ct_b64) = parse_envelope(envelope)?;
        let key_bytes = self.keys.get(key_id)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

        let nonce_bytes = BASE64
            .decode(nonce_b64)
            .map_err(|e| Error::Invalid(format!("decrypt : nonce base64 invalide : {e}")))?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(Error::Invalid(format!(
                "decrypt : nonce longueur {} (attendu {NONCE_LEN})",
                nonce_bytes.len()
            )));
        }
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ct = BASE64
            .decode(ct_b64)
            .map_err(|e| Error::Invalid(format!("decrypt : ciphertext base64 invalide : {e}")))?;

        cipher.decrypt(nonce, ct.as_ref()).map_err(|_| {
            Error::Invalid("decrypt : tag GCM invalide (corruption ou mauvaise clé)".into())
        })
    }

    /// Helper de rotation : décode `envelope` avec sa clé d'origine, ré-encode
    /// avec la clé `current()`. Renvoie l'envelope inchangée si elle est
    /// déjà sur la clé courante.
    pub fn reencrypt(&self, envelope: &str) -> Result<String> {
        let (current_key_id, _, _) = (self.keys.current()?, (), ());
        let (orig_key_id, _, _) = parse_envelope(envelope)?;
        if orig_key_id == current_key_id {
            return Ok(envelope.to_string());
        }
        let plain = self.decrypt(envelope)?;
        self.encrypt(&plain)
    }
}

/// Parse une envelope en `(key_id, nonce_b64, ciphertext_b64)`. Strict :
/// rejette toute variation de format (mauvaise version, séparateurs en trop, …).
fn parse_envelope(envelope: &str) -> Result<(&str, &str, &str)> {
    let mut parts = envelope.splitn(4, ':');
    let version = parts
        .next()
        .ok_or_else(|| Error::Invalid("envelope : vide".into()))?;
    if version != FORMAT_VERSION {
        return Err(Error::Invalid(format!(
            "envelope : version '{version}' non supportée (attendue {FORMAT_VERSION})"
        )));
    }
    let key_id = parts
        .next()
        .ok_or_else(|| Error::Invalid("envelope : key_id manquant".into()))?;
    let nonce_b64 = parts
        .next()
        .ok_or_else(|| Error::Invalid("envelope : nonce manquant".into()))?;
    let ct_b64 = parts
        .next()
        .ok_or_else(|| Error::Invalid("envelope : ciphertext manquant".into()))?;
    if key_id.is_empty() || nonce_b64.is_empty() || ct_b64.is_empty() {
        return Err(Error::Invalid("envelope : champ vide".into()));
    }
    Ok((key_id, nonce_b64, ct_b64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::keys::KEY_LEN;
    use crate::secrets::InMemoryKeyProvider;

    /// Clé de 32 octets aléatoires, différente à chaque test.
    fn random_key() -> [u8; KEY_LEN] {
        let mut k = [0u8; KEY_LEN];
        rand::thread_rng().fill_bytes(&mut k);
        k
    }

    fn cipher_with(key: [u8; KEY_LEN]) -> EnvelopeCipher<InMemoryKeyProvider> {
        let provider = InMemoryKeyProvider::with_key("k1", key);
        EnvelopeCipher::new(provider)
    }

    // ───── F6.3.b — encrypt/decrypt ─────

    #[test]
    fn roundtrip_recovers_plaintext() {
        let c = cipher_with(random_key());
        let env = c.encrypt(b"secret data").unwrap();
        assert_eq!(c.decrypt(&env).unwrap(), b"secret data");
    }

    #[test]
    fn envelope_uses_v1_format() {
        let c = cipher_with(random_key());
        let env = c.encrypt(b"x").unwrap();
        let parts: Vec<&str> = env.splitn(4, ':').collect();
        assert_eq!(parts[0], "v1");
        assert_eq!(parts[1], "k1");
        // nonce_b64 (16 chars pour 12 octets) et ciphertext_b64 (au moins 28
        // chars pour 1 octet + tag 16) sont présents
        assert!(!parts[2].is_empty());
        assert!(!parts[3].is_empty());
    }

    #[test]
    fn nonce_differs_across_encryptions() {
        let c = cipher_with(random_key());
        let env1 = c.encrypt(b"same").unwrap();
        let env2 = c.encrypt(b"same").unwrap();
        assert_ne!(env1, env2, "deux nonces frais → deux envelopes distinctes");
        // Mais les deux déchiffrent au même plaintext
        assert_eq!(c.decrypt(&env1).unwrap(), b"same");
        assert_eq!(c.decrypt(&env2).unwrap(), b"same");
    }

    #[test]
    fn empty_plaintext_roundtrips() {
        let c = cipher_with(random_key());
        let env = c.encrypt(b"").unwrap();
        assert_eq!(c.decrypt(&env).unwrap(), b"");
    }

    #[test]
    fn binary_plaintext_roundtrips() {
        let c = cipher_with(random_key());
        let payload: Vec<u8> = (0u8..=255).collect();
        let env = c.encrypt(&payload).unwrap();
        assert_eq!(c.decrypt(&env).unwrap(), payload);
    }

    // ───── tampering / mauvaise clé ─────

    #[test]
    fn ciphertext_tamper_is_detected_by_gcm_tag() {
        let c = cipher_with(random_key());
        let env = c.encrypt(b"secret").unwrap();
        // Tamper sur le ciphertext base64 : décode → flip un byte → ré-encode.
        // On évite ainsi de toucher au padding (qui produirait une erreur de
        // décodage base64 plutôt qu'un échec d'authentification AEAD).
        let (key_id, nonce_b64, ct_b64) = parse_envelope(&env).unwrap();
        let mut ct = BASE64.decode(ct_b64).unwrap();
        let mid = ct.len() / 2;
        ct[mid] ^= 0xFF;
        let tampered = format!(
            "{FORMAT_VERSION}:{key_id}:{nonce_b64}:{}",
            BASE64.encode(&ct)
        );
        let err = c.decrypt(&tampered).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("tag GCM")));
    }

    #[test]
    fn wrong_key_rejected_with_gcm_tag_error() {
        // Chiffre avec une clé, déchiffre avec une autre
        let c_a = cipher_with([1u8; 32]);
        let c_b = EnvelopeCipher::new(InMemoryKeyProvider::with_key("k1", [2u8; 32]));
        let env = c_a.encrypt(b"secret").unwrap();
        let err = c_b.decrypt(&env).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("tag GCM")));
    }

    #[test]
    fn unknown_key_id_in_envelope_errors() {
        let c = cipher_with(random_key());
        // Envelope avec un key_id que le provider ne connaît pas
        let bogus = "v1:not-a-key:AAAAAAAAAAAAAAAA:AAAAAAAAAAAAAAAAAAAAAA==";
        let err = c.decrypt(bogus).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("inconnu")));
    }

    #[test]
    fn malformed_envelope_rejected() {
        let c = cipher_with(random_key());
        for bad in ["", "v1", "v1:", "v1:k1::", "v2:k1:abc:def", "no-prefix"] {
            assert!(
                c.decrypt(bad).is_err(),
                "envelope mal formée acceptée : {bad:?}"
            );
        }
    }

    #[test]
    fn key_id_with_colon_is_refused_at_encrypt() {
        let provider = InMemoryKeyProvider::with_key("bad:id", random_key());
        let c = EnvelopeCipher::new(provider);
        let err = c.encrypt(b"x").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains(":")));
    }

    // ───── F6.3.c — rotation ─────

    #[test]
    fn rotation_keeps_old_envelopes_readable() {
        let provider = InMemoryKeyProvider::with_key("v1-key", [1u8; 32]);
        let c = EnvelopeCipher::new(provider);
        let old = c.encrypt(b"encrypted with v1-key").unwrap();
        assert!(old.starts_with("v1:v1-key:"));

        // Rotation : ajout d'une nouvelle clé devenue courante
        c.keys().add_key("v2-key", [2u8; 32], true).unwrap();
        assert_eq!(c.keys().current().unwrap(), "v2-key");

        // L'ancienne envelope reste lisible (anciennne clé toujours dispo)
        assert_eq!(c.decrypt(&old).unwrap(), b"encrypted with v1-key");

        // Un nouveau chiffrement utilise la clé courante
        let new = c.encrypt(b"encrypted with v2-key").unwrap();
        assert!(new.starts_with("v1:v2-key:"));
        assert_eq!(c.decrypt(&new).unwrap(), b"encrypted with v2-key");
    }

    #[test]
    fn reencrypt_moves_envelope_to_current_key() {
        let provider = InMemoryKeyProvider::with_key("k1", [1u8; 32]);
        let c = EnvelopeCipher::new(provider);
        let env = c.encrypt(b"data").unwrap();
        assert!(env.starts_with("v1:k1:"));

        c.keys().add_key("k2", [2u8; 32], true).unwrap();
        let rotated = c.reencrypt(&env).unwrap();

        assert!(rotated.starts_with("v1:k2:"));
        assert_eq!(c.decrypt(&rotated).unwrap(), b"data");
    }

    #[test]
    fn reencrypt_no_op_when_already_on_current_key() {
        let c = cipher_with(random_key());
        let env = c.encrypt(b"x").unwrap();
        let same = c.reencrypt(&env).unwrap();
        assert_eq!(env, same);
    }

    #[test]
    fn reencrypt_after_removing_old_key_fails() {
        let provider = InMemoryKeyProvider::with_key("k1", [1u8; 32]);
        let c = EnvelopeCipher::new(provider);
        let env = c.encrypt(b"data").unwrap();

        c.keys().add_key("k2", [2u8; 32], true).unwrap();
        c.keys().remove_key("k1").unwrap();

        // L'ancienne envelope est définitivement perdue
        assert!(c.decrypt(&env).is_err());
        assert!(c.reencrypt(&env).is_err());
    }

    #[test]
    fn random_key_returns_distinct_keys() {
        let a = random_key();
        let b = random_key();
        assert_ne!(a, b);
    }
}

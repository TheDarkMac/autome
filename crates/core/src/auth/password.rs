//! Hash / vérification de mots de passe (F6.1.a).
//!
//! Le contrat [`PasswordHasher`] est minimal et agnostique : `hash` produit
//! une **chaîne PHC** (format auto-descriptif `$algo$params$salt$hash`),
//! `verify` consomme cette même chaîne pour valider un plain-text.
//!
//! L'impl par défaut [`Argon2PasswordHasher`] utilise Argon2id avec les
//! paramètres recommandés par RFC 9106 (catégorie « deuxième usage » :
//! 19 MiB, 2 iterations, 1 lane) — un bon compromis sécurité/latence sur
//! serveur Web (~50 ms par hash sur du matériel moderne).
//!
//! Le sel est généré par [`rand::rngs::OsRng`] (CSPRNG OS) à chaque
//! hash ; il est inclus dans la chaîne PHC retournée.

use argon2::Argon2;
use password_hash::{
    rand_core::OsRng, Error as PhcError, PasswordHash, PasswordHasher as PhcHasher,
    PasswordVerifier, SaltString,
};

use crate::{Error, Result};

/// Contrat d'un hashing de mot de passe.
pub trait PasswordHasher {
    /// Hash `plain` en une chaîne PHC (auto-descriptive).
    fn hash(&self, plain: &str) -> Result<String>;

    /// Vérifie que `plain` correspond à `phc_hash`.
    /// Retourne `Ok(true)` si match, `Ok(false)` sinon. `Err` seulement
    /// si la chaîne PHC est mal formée.
    fn verify(&self, plain: &str, phc_hash: &str) -> Result<bool>;
}

/// Implémentation Argon2id (paramètres par défaut du crate `argon2`).
#[derive(Debug, Default, Clone)]
pub struct Argon2PasswordHasher {
    argon: Argon2<'static>,
}

impl Argon2PasswordHasher {
    /// Construit le hasher avec les paramètres par défaut.
    pub fn new() -> Self {
        Self::default()
    }
}

fn map_phc(err: PhcError) -> Error {
    Error::Invalid(format!("argon2 : {err}"))
}

impl PasswordHasher for Argon2PasswordHasher {
    fn hash(&self, plain: &str) -> Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = self
            .argon
            .hash_password(plain.as_bytes(), &salt)
            .map_err(map_phc)?
            .to_string();
        Ok(hash)
    }

    fn verify(&self, plain: &str, phc_hash: &str) -> Result<bool> {
        let parsed = PasswordHash::new(phc_hash).map_err(map_phc)?;
        match self.argon.verify_password(plain.as_bytes(), &parsed) {
            Ok(()) => Ok(true),
            Err(PhcError::Password) => Ok(false),
            Err(e) => Err(map_phc(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_succeeds() {
        let h = Argon2PasswordHasher::new();
        let phc = h.hash("correct horse battery staple").unwrap();
        assert!(phc.starts_with("$argon2"));
        assert!(h.verify("correct horse battery staple", &phc).unwrap());
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let h = Argon2PasswordHasher::new();
        let phc = h.hash("hunter2").unwrap();
        assert!(!h.verify("hunter3", &phc).unwrap());
    }

    #[test]
    fn hashes_are_salted_per_call() {
        let h = Argon2PasswordHasher::new();
        let a = h.hash("same").unwrap();
        let b = h.hash("same").unwrap();
        assert_ne!(a, b, "le sel doit différer à chaque hash");
        // Mais les deux vérifient le même plain
        assert!(h.verify("same", &a).unwrap());
        assert!(h.verify("same", &b).unwrap());
    }

    #[test]
    fn malformed_phc_returns_error() {
        let h = Argon2PasswordHasher::new();
        let err = h.verify("x", "pas un hash PHC").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("argon2")));
    }

    #[test]
    fn unicode_password_works() {
        let h = Argon2PasswordHasher::new();
        let phc = h.hash("mot de passe avec émoji 🔐 et accent é").unwrap();
        assert!(h
            .verify("mot de passe avec émoji 🔐 et accent é", &phc)
            .unwrap());
    }

    #[test]
    fn empty_password_is_supported() {
        // Pas idéal côté UX, mais le hasher ne doit pas paniquer.
        let h = Argon2PasswordHasher::new();
        let phc = h.hash("").unwrap();
        assert!(h.verify("", &phc).unwrap());
        assert!(!h.verify("x", &phc).unwrap());
    }
}

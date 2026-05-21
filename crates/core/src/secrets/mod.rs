//! Chiffrement des credentials/secrets au repos (F6.3).
//!
//! Voir [ADR 0004](../../../../docs/adr/0004-chiffrement-secrets-repos.md)
//! pour le détail du choix : AES-256-GCM avec un [`KeyProvider`] versionné,
//! format envelope auto-descriptif `v1:<key_id>:<nonce>:<ct+tag>`, rotation
//! native via ajout d'une nouvelle clé (les anciennes restant accessibles
//! pour `decrypt`).
//!
//! Modules :
//! - [`keys`] — trait `KeyProvider` + impl `InMemoryKeyProvider` ;
//! - [`cipher`] — `EnvelopeCipher::encrypt` / `decrypt` / `reencrypt`.

pub mod cipher;
pub mod keys;

pub use cipher::EnvelopeCipher;
pub use keys::{InMemoryKeyProvider, KeyProvider};

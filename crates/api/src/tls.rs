//! Configuration TLS (F6.4) — chargement PEM + `axum_server::RustlsConfig`.
//!
//! Lit deux fichiers PEM (cert chain + clé privée) et les transforme en
//! `RustlsConfig` consommable par `axum_server::bind_rustls`. Erreurs
//! typées pour distinguer fichier manquant, PEM malformé, paire
//! cert/key incompatible.
//!
//! Le câblage côté `main` (branchement HTTP vs HTTPS selon variables
//! d'env) vit dans `src/main.rs` (F6.4.b).
//!
//! ## Format attendu
//!
//! - **Cert chain** : un ou plusieurs blocs `-----BEGIN CERTIFICATE-----`
//!   concaténés (feuille en premier, racine en dernier).
//! - **Clé privée** : bloc `-----BEGIN PRIVATE KEY-----` (PKCS#8),
//!   `-----BEGIN RSA PRIVATE KEY-----` (PKCS#1) ou
//!   `-----BEGIN EC PRIVATE KEY-----` (SEC1). Tous les trois sont acceptés
//!   par `rustls-pemfile`.

use std::path::Path;
use std::sync::OnceLock;

use axum_server::tls_rustls::RustlsConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use autome_core::{Error, Result};

/// Installation idempotente du `CryptoProvider` rustls. Appelée à chaque
/// entrée dans `load_rustls_config` — `install_default` lui-même est
/// idempotent (échoue silencieusement si déjà appelé), mais le `OnceLock`
/// évite la prise de lock interne sur les appels répétés.
static PROVIDER_INSTALLED: OnceLock<()> = OnceLock::new();

fn ensure_crypto_provider() {
    PROVIDER_INSTALLED.get_or_init(|| {
        // L'erreur ne peut survenir que si un autre provider a déjà été
        // installé dans le même process — c'est correct, on ignore.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Charge cert chain + clé privée depuis deux fichiers PEM et construit
/// un [`RustlsConfig`] prêt pour `axum_server::bind_rustls`.
///
/// Tâche `async` car `RustlsConfig::from_der` est `async` côté axum-server
/// (lazy init du `CryptoProvider`).
pub async fn load_rustls_config(
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
) -> Result<RustlsConfig> {
    ensure_crypto_provider();
    let certs = load_cert_chain(cert_path.as_ref())?;
    let key = load_private_key(key_path.as_ref())?;
    if certs.is_empty() {
        return Err(Error::Invalid(format!(
            "tls : aucun certificat trouvé dans '{}'",
            cert_path.as_ref().display()
        )));
    }
    // `RustlsConfig::from_der` accepte les `CertificateDer` directement.
    let cert_der: Vec<Vec<u8>> = certs.into_iter().map(|c| c.as_ref().to_vec()).collect();
    let key_der = key.secret_der().to_vec();
    RustlsConfig::from_der(cert_der, key_der)
        .await
        .map_err(|e| Error::Invalid(format!("tls : ServerConfig invalide : {e}")))
}

/// Lit et parse une chaîne PEM `-----BEGIN CERTIFICATE-----` depuis `path`.
pub fn load_cert_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let bytes = std::fs::read(path).map_err(|e| {
        Error::Invalid(format!(
            "tls : impossible de lire le cert '{}' : {e}",
            path.display()
        ))
    })?;
    let mut cursor = std::io::Cursor::new(bytes);
    rustls_pemfile::certs(&mut cursor)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Invalid(format!("tls : cert PEM invalide : {e}")))
}

/// Lit la première clé privée PEM trouvée dans `path` (PKCS#8, PKCS#1, SEC1).
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let bytes = std::fs::read(path).map_err(|e| {
        Error::Invalid(format!(
            "tls : impossible de lire la clé '{}' : {e}",
            path.display()
        ))
    })?;
    let mut cursor = std::io::Cursor::new(bytes);
    rustls_pemfile::private_key(&mut cursor)
        .map_err(|e| Error::Invalid(format!("tls : clé PEM invalide : {e}")))?
        .ok_or_else(|| {
            Error::Invalid(format!(
                "tls : aucune clé privée PEM trouvée dans '{}'",
                path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Génère une paire cert+key self-signed via `rcgen` et écrit les
    /// deux PEM dans des fichiers temporaires.
    fn write_self_signed_pair() -> (NamedTempFile, NamedTempFile) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("rcgen self-signed");
        let cert_pem = cert.cert.pem();
        let key_pem = cert.key_pair.serialize_pem();

        let mut cert_file = NamedTempFile::new().unwrap();
        cert_file.write_all(cert_pem.as_bytes()).unwrap();
        let mut key_file = NamedTempFile::new().unwrap();
        key_file.write_all(key_pem.as_bytes()).unwrap();
        (cert_file, key_file)
    }

    #[test]
    fn load_cert_chain_reads_self_signed() {
        let (cert_f, _key_f) = write_self_signed_pair();
        let chain = load_cert_chain(cert_f.path()).unwrap();
        assert_eq!(chain.len(), 1, "self-signed → un seul certif");
    }

    #[test]
    fn load_cert_chain_errors_on_missing_file() {
        let err = load_cert_chain(Path::new("/nonexistent/path/cert.pem")).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("impossible de lire")));
    }

    #[test]
    fn load_cert_chain_errors_on_garbage_content() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"not a PEM file at all").unwrap();
        // `rustls_pemfile::certs` retourne un iterateur vide pour un input
        // non-PEM ; pas une erreur de parsing. On vérifie via le check
        // `certs.is_empty()` côté `load_rustls_config`.
        let chain = load_cert_chain(f.path()).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn load_private_key_reads_self_signed() {
        let (_cert_f, key_f) = write_self_signed_pair();
        let _key = load_private_key(key_f.path()).unwrap();
        // Succès = on a une clé ; rustls fait sa propre validation au build
        // du ServerConfig (load_rustls_config), pas ici.
    }

    #[test]
    fn load_private_key_errors_on_missing_file() {
        let err = load_private_key(Path::new("/nope/key.pem")).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("impossible de lire")));
    }

    #[test]
    fn load_private_key_errors_when_no_pem_block() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"garbage without PEM blocks").unwrap();
        let err = load_private_key(f.path()).unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("aucune clé privée")));
    }

    #[tokio::test]
    async fn load_rustls_config_builds_from_self_signed() {
        let (cert_f, key_f) = write_self_signed_pair();
        let _config = load_rustls_config(cert_f.path(), key_f.path())
            .await
            .expect("RustlsConfig from self-signed");
    }

    #[tokio::test]
    async fn load_rustls_config_errors_on_empty_cert_file() {
        let cert_f = NamedTempFile::new().unwrap();
        let (_, key_f) = write_self_signed_pair();
        let err = load_rustls_config(cert_f.path(), key_f.path())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("aucun certificat")));
    }

    #[tokio::test]
    async fn load_rustls_config_errors_on_missing_cert_file() {
        let (_, key_f) = write_self_signed_pair();
        let err = load_rustls_config("/no/such/cert.pem", key_f.path())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.contains("impossible de lire")));
    }
}

//! Test d'intégration HTTPS bout-en-bout (F6.4.c).
//!
//! Génère un cert self-signed via `rcgen`, démarre un serveur axum
//! HTTPS sur un port éphémère, fait un GET `/health` via `reqwest`
//! (configuré avec le cert comme root) et vérifie la 200.
//!
//! `#[ignore]` retiré : le test est suffisamment isolé (port `:0`, temp
//! files, timeout strict) pour run par défaut.

use std::io::Write;
use std::net::SocketAddr;
use std::time::Duration;

use autome_api::tls::load_rustls_config;
use axum::{routing::get, Router};
use tempfile::NamedTempFile;

fn write_self_signed_pair_for(hostname: &str) -> (NamedTempFile, NamedTempFile, String) {
    let cert =
        rcgen::generate_simple_self_signed(vec![hostname.to_string()]).expect("rcgen self-signed");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    let mut cert_file = NamedTempFile::new().unwrap();
    cert_file.write_all(cert_pem.as_bytes()).unwrap();
    let mut key_file = NamedTempFile::new().unwrap();
    key_file.write_all(key_pem.as_bytes()).unwrap();
    (cert_file, key_file, cert_pem)
}

#[tokio::test]
async fn https_round_trip_with_self_signed_cert() {
    // 1. Cert self-signed pour `localhost`
    let (cert_f, key_f, cert_pem) = write_self_signed_pair_for("localhost");

    // 2. Charge la config TLS via notre loader
    let tls_config = load_rustls_config(cert_f.path(), key_f.path())
        .await
        .expect("load_rustls_config");

    // 3. App axum minimale : `/health → "ok"`
    let app: Router = Router::new().route("/health", get(|| async { "ok" }));

    // 4. Démarre le serveur sur port éphémère (système choisit)
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    drop(listener); // libère le port — axum-server va le re-binder

    let server_handle = axum_server::Handle::new();
    let handle_for_serve = server_handle.clone();
    let server = tokio::spawn(async move {
        axum_server::bind_rustls(addr, tls_config)
            .handle(handle_for_serve)
            .serve(app.into_make_service())
            .await
            .expect("axum_server serve");
    });

    // 5. Attend que le serveur soit prêt (handle.listening() résout quand
    // bind() a réussi). Timeout de garde pour ne pas bloquer la CI.
    let listening = tokio::time::timeout(Duration::from_secs(5), server_handle.listening())
        .await
        .expect("serveur prêt dans les 5s");
    let bound = listening.expect("listening returned an addr");

    // 6. Client reqwest configuré pour faire confiance à notre cert
    let cert = reqwest::Certificate::from_pem(cert_pem.as_bytes()).expect("reqwest::Certificate");
    let client = reqwest::Client::builder()
        .add_root_certificate(cert)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");

    // 7. GET https://<bound>/health
    // Cert émis pour `localhost` → on connecte via le hostname, pas l'IP.
    let url = format!("https://localhost:{}/health", bound.port());
    let response = client.get(&url).send().await.expect("HTTPS GET");
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert_eq!(body, "ok");

    // 8. Arrêt propre
    server_handle.graceful_shutdown(Some(Duration::from_secs(1)));
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

#[tokio::test]
async fn https_fails_without_root_cert_trust() {
    // Même setup mais le client ne fait *pas* confiance au cert → erreur TLS.
    let (cert_f, key_f, _cert_pem) = write_self_signed_pair_for("localhost");
    let tls_config = load_rustls_config(cert_f.path(), key_f.path())
        .await
        .unwrap();

    let app: Router = Router::new().route("/health", get(|| async { "ok" }));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    drop(listener);

    let server_handle = axum_server::Handle::new();
    let handle_for_serve = server_handle.clone();
    tokio::spawn(async move {
        let _ = axum_server::bind_rustls(addr, tls_config)
            .handle(handle_for_serve)
            .serve(app.into_make_service())
            .await;
    });
    let bound = tokio::time::timeout(Duration::from_secs(5), server_handle.listening())
        .await
        .expect("ready")
        .expect("addr");

    // Client sans `add_root_certificate` → reqwest rejette la connexion.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    // Cert émis pour `localhost` → on connecte via le hostname, pas l'IP.
    let url = format!("https://localhost:{}/health", bound.port());
    let result = client.get(&url).send().await;
    assert!(result.is_err(), "le client doit rejeter le cert non-trusté");

    server_handle.graceful_shutdown(Some(Duration::from_secs(1)));
}

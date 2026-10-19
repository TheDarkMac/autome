//! #233 — le client filtré ignore les proxys de l'environnement.
//!
//! Binaire de test à part, avec un seul test : il modifie l'environnement du
//! processus (`HTTP(S)_PROXY`, `ALL_PROXY`) avant de construire le client.
//! Avec un proxy, seul le nom du proxy passerait par le résolveur filtré : la
//! cible serait envoyée au proxy, qui la résoudrait sans contrôle.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use autome_connectors::{EgressPolicy, HttpClient, HttpRequest, ReqwestHttpClient};

/// Serveur local qui répond 200 à chaque connexion et les compte.
fn serve() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
        }
    });
    (port, hits)
}

fn get(client: &ReqwestHttpClient, url: &str) -> Result<u16, String> {
    client
        .send(&HttpRequest {
            method: "GET".into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
        })
        .map(|r| r.status)
}

#[test]
fn environment_proxy_is_ignored_and_target_stays_filtered() {
    let (proxy_port, proxy_hits) = serve();
    let (target_port, target_hits) = serve();
    let proxy = format!("http://127.0.0.1:{proxy_port}");
    for var in autome_connectors::PROXY_ENV_VARS {
        std::env::set_var(var, &proxy);
    }

    // Cible interne par son nom : refusée par le résolveur, rien ne part
    // vers le proxy.
    let strict = ReqwestHttpClient::new();
    let err = get(&strict, &format!("http://localhost:{target_port}/")).unwrap_err();
    assert!(err.contains("egress refusé"), "{err}");
    assert!(err.contains("non publique"), "{err}");
    // Nom interne inconnu du DNS public : le proxy l'aurait résolu lui-même.
    assert!(get(&strict, "http://metadata.google.internal/").is_err());

    // Cible autorisée par l'opérateur : jointe en direct, pas via le proxy.
    let allowed = ReqwestHttpClient::with_policy(EgressPolicy::with_allowed_hosts(["localhost"]));
    assert_eq!(
        get(&allowed, &format!("http://localhost:{target_port}/")),
        Ok(200)
    );

    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        0,
        "le proxy ne doit rien recevoir"
    );
    assert_eq!(target_hits.load(Ordering::SeqCst), 1);
}

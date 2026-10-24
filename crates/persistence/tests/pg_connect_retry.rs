//! Connexion de démarrage retentée (#273) contre un vrai PostgreSQL.
//! Gaté par `TEST_DATABASE_URL` (skip-pass sinon). La base « démarre après
//! l'API » grâce à un relais TCP ouvert en retard devant le vrai serveur.

use std::time::{Duration, Instant};

use autome_persistence::connect_retrying;
use tokio::net::{TcpListener, TcpStream};
use tokio_postgres::config::Host;

/// DSN découpé, pour en changer l'hôte, le rôle ou la base.
struct Dsn {
    scheme: String,
    userinfo: Option<String>,
    hostport: String,
    database: String,
    query: String,
}

impl Dsn {
    fn parse(url: &str) -> Self {
        let (scheme, rest) = url.split_once("://").expect("schéma");
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h.to_string()),
            None => (None, authority.to_string()),
        };
        let tail = tail.strip_prefix('/').unwrap_or(tail);
        let (database, query) = match tail.split_once('?') {
            Some((d, q)) => (d.to_string(), format!("?{q}")),
            None => (tail.to_string(), String::new()),
        };
        Self {
            scheme: scheme.to_string(),
            userinfo,
            hostport,
            database,
            query,
        }
    }

    fn url(&self) -> String {
        let user = self
            .userinfo
            .as_ref()
            .map(|u| format!("{u}@"))
            .unwrap_or_default();
        format!(
            "{}://{user}{}/{}{}",
            self.scheme, self.hostport, self.database, self.query
        )
    }
}

/// Adresse TCP du vrai serveur.
fn upstream(url: &str) -> (String, u16) {
    let config: tokio_postgres::Config = url.parse().expect("DSN");
    let host = match config.get_hosts().first() {
        Some(Host::Tcp(h)) => h.clone(),
        _ => "localhost".to_string(),
    };
    (host, config.get_ports().first().copied().unwrap_or(5432))
}

/// Relais `127.0.0.1:port` → `upstream`, ouvert au bout de `delay`.
fn relay_after(delay: Duration, port: u16, upstream: (String, u16)) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        loop {
            let (mut client, _) = listener.accept().await.unwrap();
            let target = upstream.clone();
            tokio::spawn(async move {
                let mut server = TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
            });
        }
    });
}

#[tokio::test]
async fn a_database_that_starts_after_the_api_is_awaited() {
    let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    // Port réservé puis libéré : connexion refusée jusqu'au relais.
    let port = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut dsn = Dsn::parse(&url);
    dsn.hostport = format!("127.0.0.1:{port}");
    relay_after(Duration::from_millis(1200), port, upstream(&url));

    let started = Instant::now();
    let pool = connect_retrying(&dsn.url(), Duration::from_secs(30))
        .await
        .expect("base joignable une fois démarrée");
    assert!(started.elapsed() >= Duration::from_millis(1200));
    let one: i32 = pool
        .get()
        .await
        .unwrap()
        .query_one("SELECT 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(one, 1);
}

#[tokio::test]
async fn permanent_errors_are_not_retried() {
    let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
        eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
        return;
    };
    // Base absente (3D000) ; rôle absent (28000 en `trust`, 28P01 sinon).
    let mut missing_db = Dsn::parse(&url);
    missing_db.database = "autome_base_absente_273".into();
    let mut missing_role = Dsn::parse(&url);
    missing_role.userinfo = Some("autome_role_absent_273:x".into());

    for (dsn, codes) in [
        (missing_db, &["3D000"][..]),
        (missing_role, &["28000", "28P01"][..]),
    ] {
        let started = Instant::now();
        let err = connect_retrying(&dsn.url(), Duration::from_secs(30))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "erreur définitive rendue aussitôt : {err}"
        );
        assert!(codes.iter().any(|c| err.contains(c)), "{err}");
    }
}

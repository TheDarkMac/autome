//! Pool de connexions PostgreSQL **async** (deadpool + tokio-postgres).

use std::error::Error as _;
use std::time::Duration;
use std::{fmt, io};

use autome_core::{Error, Result};
use deadpool_postgres::{Config as DeadpoolConfig, Pool, PoolError, Runtime};
use tokio_postgres::error::SqlState;
use tokio_postgres::NoTls;

/// Pool de connexions PostgreSQL partageable (`Clone`, `Send + Sync`).
///
/// `NoTls` : suffisant pour un trafic intra-cluster (services ↔ Postgres dans
/// le même namespace k8s). Le chiffrement du lien pourra être ajouté plus tard.
pub type PgPool = Pool;

/// Construit un pool à partir d'un DSN (`postgres://user:pass@host/db`).
///
/// La connexion n'est pas ouverte ici : deadpool établit les connexions à la
/// demande (`pool.get().await`). Les erreurs de connexion remontent au premier
/// usage, avec un message clair.
pub fn connect(database_url: &str) -> Result<PgPool> {
    let cfg = DeadpoolConfig {
        url: Some(database_url.to_string()),
        ..DeadpoolConfig::new()
    };
    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| Error::Invalid(format!("storage: création du pool PostgreSQL : {e}")))
}

/// Comme [`connect`], puis attend que la base accepte une connexion (#273),
/// en retentant pendant `timeout` (`0` = une seule tentative) les erreurs
/// **transitoires** : réseau (connexion refusée ou réinitialisée, nom pas
/// encore résolu — Service headless sans pod prêt) et serveur en cours de
/// démarrage (`57P03`). Ce que le serveur refuse (authentification, rôle ou
/// base inexistants…) est définitif et remonte aussitôt. La connexion
/// ouverte reste dans le pool.
pub async fn connect_retrying(database_url: &str, timeout: Duration) -> Result<PgPool> {
    let pool = connect(database_url)?;
    crate::retry::retry_until(
        "postgres",
        timeout,
        Duration::from_millis(500),
        Duration::from_secs(5),
        ConnectError::is_transient,
        || async { pool.get().await.map(drop).map_err(ConnectError) },
    )
    .await
    .map_err(|e| Error::Invalid(format!("storage: connexion PostgreSQL : {e}")))?;
    Ok(pool)
}

/// Échec d'ouverture d'une connexion du pool, affiché avec sa cause :
/// tokio-postgres n'écrit que « error connecting to server » et garde
/// l'erreur réseau ou la réponse du serveur dans `source()`.
struct ConnectError(PoolError);

impl ConnectError {
    fn is_transient(&self) -> bool {
        let PoolError::Backend(e) = &self.0 else {
            return false;
        };
        match e.code() {
            Some(code) => *code == SqlState::CANNOT_CONNECT_NOW,
            None => e.is_closed() || e.source().is_some_and(|c| c.is::<io::Error>()),
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let PoolError::Backend(e) = &self.0 else {
            return Ok(());
        };
        let mut cause = e.source();
        while let Some(c) = cause {
            write!(f, " : {c}")?;
            cause = c.source();
        }
        if let Some(code) = e.code() {
            write!(f, " (SQLSTATE {})", code.code())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Réponse du faux serveur à une connexion.
    #[derive(Clone, Copy)]
    enum Reply {
        /// `ErrorResponse` FATAL portant ce SQLSTATE, puis fermeture.
        Fatal(&'static str),
        /// Authentification acceptée, session prête.
        Ready,
    }

    /// Port libre, sans personne à l'écoute (connexion refusée).
    async fn closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    fn url(port: u16) -> String {
        format!("postgres://autome@127.0.0.1:{port}/autome")
    }

    /// Lit un message de démarrage (longueur incluse) ; `true` si SSLRequest.
    async fn read_startup(sock: &mut TcpStream) -> bool {
        let len = sock.read_i32().await.unwrap() as usize;
        let mut body = vec![0; len - 4];
        sock.read_exact(&mut body).await.unwrap();
        body[..4] == 80_877_103i32.to_be_bytes()
    }

    /// Message du protocole : type, longueur (incluse), contenu.
    fn message(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    async fn answer(mut sock: TcpStream, reply: Reply) {
        if read_startup(&mut sock).await {
            sock.write_all(b"N").await.unwrap();
            read_startup(&mut sock).await;
        }
        match reply {
            Reply::Fatal(code) => {
                let mut body = Vec::new();
                for (field, value) in [(b'S', "FATAL"), (b'C', code), (b'M', "refus du test")] {
                    body.push(field);
                    body.extend_from_slice(value.as_bytes());
                    body.push(0);
                }
                body.push(0);
                sock.write_all(&message(b'E', &body)).await.unwrap();
            }
            Reply::Ready => {
                sock.write_all(&message(b'R', &0i32.to_be_bytes()))
                    .await
                    .unwrap();
                sock.write_all(&message(b'Z', b"I")).await.unwrap();
                // Session gardée ouverte jusqu'à la déconnexion du client.
                let _ = sock.read(&mut [0; 1024]).await;
            }
        }
    }

    /// Faux PostgreSQL sur `port` : la connexion n°i reçoit `replies[i]` (la
    /// dernière réponse se répète). Renvoie le compteur de connexions.
    async fn fake_server(port: u16, replies: Vec<Reply>) -> Arc<AtomicU32> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let accepted = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let n = counter.fetch_add(1, Ordering::SeqCst) as usize;
                let reply = replies[n.min(replies.len() - 1)];
                tokio::spawn(answer(sock, reply));
            }
        });
        accepted
    }

    #[tokio::test]
    async fn a_refused_connection_is_retried_until_the_timeout() {
        let port = closed_port().await;
        let started = Instant::now();
        let err = connect_retrying(&url(port), Duration::from_millis(700))
            .await
            .unwrap_err()
            .to_string();
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(450), "retenté : {waited:?}");
        assert!(waited < Duration::from_secs(3), "délai borné : {waited:?}");
        assert!(err.contains("connexion PostgreSQL"), "{err}");
        // La cause (io) est dans le message, pas seulement « error connecting ».
        assert!(err.to_lowercase().contains("refused"), "{err}");
    }

    #[tokio::test]
    async fn a_database_that_starts_late_is_awaited() {
        let port = closed_port().await;
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            fake_server(port, vec![Reply::Ready]).await
        });
        let started = Instant::now();
        let pool = connect_retrying(&url(port), Duration::from_secs(30))
            .await
            .expect("base joignable après son démarrage");
        assert!(started.elapsed() >= Duration::from_millis(600));
        let accepted = late.await.unwrap();
        // La connexion ouverte pendant l'attente reste dans le pool.
        assert_eq!(pool.status().size, 1);
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_server_still_starting_up_is_retried() {
        // 57P03 « the database system is starting up » : port ouvert, base
        // pas encore prête ⇒ transitoire.
        let port = closed_port().await;
        let accepted = fake_server(
            port,
            vec![Reply::Fatal("57P03"), Reply::Fatal("57P03"), Reply::Ready],
        )
        .await;
        connect_retrying(&url(port), Duration::from_secs(30))
            .await
            .expect("prête à la troisième tentative");
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn authentication_and_missing_database_are_not_retried() {
        // 28P01 mot de passe refusé, 28000 rôle inconnu, 3D000 base absente.
        for code in ["28P01", "28000", "3D000"] {
            let port = closed_port().await;
            let accepted = fake_server(port, vec![Reply::Fatal(code)]).await;
            let started = Instant::now();
            let err = connect_retrying(&url(port), Duration::from_secs(30))
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(
                accepted.load(Ordering::SeqCst),
                1,
                "{code} : aucune relance"
            );
            assert!(started.elapsed() < Duration::from_secs(2), "{code}");
            assert!(err.contains(code), "SQLSTATE dans le message : {err}");
            assert!(err.contains("refus du test"), "{err}");
        }
    }

    #[tokio::test]
    async fn a_zero_timeout_makes_a_single_attempt() {
        let port = closed_port().await;
        let started = Instant::now();
        connect_retrying(&url(port), Duration::ZERO)
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(400));
    }
}

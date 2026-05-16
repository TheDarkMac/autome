//! `autome-api` — serveur HTTP (axum).
//!
//! Conformément à l'ADR 0001, axum est confiné à ce crate ; `core` et
//! `engine` n'en dépendent pas. Squelette F1.3.d : un routeur avec une
//! sonde de santé. Le CRUD workflows et le déclenchement de runs relèvent
//! du domaine 7 (F7.*).

use autome_core::Config;
use axum::{routing::get, Router};

/// Handler de la sonde de santé.
async fn health() -> &'static str {
    "ok"
}

/// Construit le routeur de l'API.
///
/// Isolé de `main` pour être testable sans démarrer de serveur.
fn build_router() -> Router {
    Router::new().route("/health", get(health))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let app = build_router();
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await?;
    println!(
        "autome-api ({:?}) en écoute sur http://{}",
        config.profile, config.http_addr
    );
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_returns_ok() {
        assert_eq!(health().await, "ok");
    }

    #[test]
    fn router_builds() {
        // Le routeur se construit sans panique (route /health enregistrée).
        let _app: Router = build_router();
    }
}

//! `autome-api` — binaire du serveur HTTP (axum).
//!
//! Conformément à l'ADR 0001, axum est confiné à ce crate ; `core` et
//! `engine` n'en dépendent pas. Squelette F1.3.d : un routeur avec une
//! sonde de santé. Le CRUD workflows et le déclenchement de runs relèvent
//! du domaine 7 (F7.*). Le middleware d'authentification (F6.1.c) vit dans
//! [`autome_api::auth`].

use std::sync::Arc;

use autome_api::auth::{protected_router, AuthState};
use autome_api::tls::load_rustls_config;
use autome_core::{
    AuditSink, Config, InMemoryPermissionStore, InMemoryRevocationStore, NoopAuditSink,
    PermissionStore, RevocationStore, TokenService,
};
use axum::{routing::get, Router};

/// Handler de la sonde de santé.
async fn health() -> &'static str {
    "ok"
}

/// Construit le routeur racine — `/health` public + `/auth/*` protégé par JWT.
///
/// Isolé de `main` pour être testable sans démarrer de serveur.
fn build_router(auth: AuthState) -> Router {
    Router::new()
        .route("/health", get(health))
        .nest("/auth", protected_router(auth))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;

    // Secret JWT depuis la config (F1.6) — à remplacer par une vraie clé
    // dérivée d'env quand on aura `config.jwt_secret`. Pour le squelette
    // on prend un secret dev déterministe ; **ne pas livrer en prod**.
    let jwt_secret = b"DEV-ONLY-replace-with-config-secret-please-32";
    let revocation: Arc<dyn RevocationStore + Send + Sync> =
        Arc::new(InMemoryRevocationStore::new());
    let permissions: Arc<dyn PermissionStore + Send + Sync> =
        Arc::new(InMemoryPermissionStore::new());
    let audit: Arc<dyn AuditSink + Send + Sync> = Arc::new(NoopAuditSink);
    let auth = AuthState::new(
        TokenService::new(jwt_secret),
        revocation,
        permissions,
        audit,
    );

    let app = build_router(auth);

    // F6.4 — HTTPS via rustls si `AUTOME_TLS_CERT` + `AUTOME_TLS_KEY`
    // sont positionnées ; sinon HTTP en clair. Décision all-or-nothing
    // (pas de fallback silencieux qui dégraderait la sécurité).
    let tls_cert = std::env::var("AUTOME_TLS_CERT").ok();
    let tls_key = std::env::var("AUTOME_TLS_KEY").ok();
    match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let tls_config = load_rustls_config(&cert, &key).await?;
            let addr: std::net::SocketAddr = config.http_addr.parse()?;
            println!(
                "autome-api ({:?}) en écoute sur https://{}  (TLS rustls)",
                config.profile, addr
            );
            axum_server::bind_rustls(addr, tls_config)
                .serve(app.into_make_service())
                .await?;
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(&config.http_addr).await?;
            println!(
                "autome-api ({:?}) en écoute sur http://{}  (sans TLS)",
                config.profile, config.http_addr
            );
            axum::serve(listener, app).await?;
        }
        _ => {
            return Err(
                "TLS partiellement configuré : positionnez AUTOME_TLS_CERT *et* AUTOME_TLS_KEY"
                    .into(),
            );
        }
    }
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
        // Le routeur se construit sans panique (route /health + /auth/*).
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let permissions: Arc<dyn PermissionStore + Send + Sync> =
            Arc::new(InMemoryPermissionStore::new());
        let audit: Arc<dyn AuditSink + Send + Sync> = Arc::new(NoopAuditSink);
        let auth = AuthState::new(TokenService::new(b"x"), revocation, permissions, audit);
        let _app: Router = build_router(auth);
    }
}

//! Sélection du backend de persistance au démarrage (dette #107, constat 2).
//!
//! Le mode est piloté par la variable d'environnement `AUTOME_PERSISTENCE` :
//! - `memory` (défaut) : stores in-process (dev, tests, single-node) ;
//! - `postgres` : stores adossés à PostgreSQL (`autome-persistence`), partagés
//!   entre répliques et persistés.
//!
//! En `postgres`, l'[`AuthState`] est bâti sur `PgRevocationStore` /
//! `PgPermissionStore` / `PgAuditSink` (pool unique partagé). Le `TokenService`
//! vient toujours de `config.jwt_secret` (constat 1).

use std::sync::Arc;

use autome_connectors::oauth::{
    ConnectorTokenStore, InMemoryConnectorTokenStore, OAuthClient, OAuthRegistry,
};
use autome_connectors::{
    CredentialService, EgressPolicy, OAuthCredentialService, ReqwestHttpClient,
};
use autome_core::{
    AccountEraser, Config, InMemoryLoginThrottle, InMemoryMfaStore, InMemoryPermissionStore,
    InMemoryRevocationStore, InMemoryRunStateStore, InMemoryTriggerScheduleStore,
    InMemoryUserStore, InMemoryWebhookEndpointStore, InMemoryWorkflowRepository,
    LoginThrottleStore, MfaStore, NoopAuditSink, Queue, QueueRunDispatcher, Result,
    RevocationStore, RunDispatcher, RunStateStore, TriggerScheduleStore, UserStore,
    WebhookEndpointStore, WorkflowRepository,
};
use autome_persistence::{
    connect, connector_cipher, run_migrations, PgAccountEraser, PgAuditSink, PgConnectorTokenStore,
    PgLoginThrottle, PgMfaStore, PgPermissionStore, PgRevocationStore, PgRunStateStore,
    PgTriggerScheduleStore, PgUserStore, PgWebhookEndpointStore, PgWorkflowRepository,
};

use crate::auth::{token_service_from_config, AuthState};
use crate::erasure::InMemoryAccountEraser;

// Sélecteurs de file/stores partagés avec le worker (dette #107).
pub use autome_persistence::{
    build_health_checks, build_queue, ensure_durable_backends, PersistenceMode, QueueMode,
};

/// Construit le service de credentials OAuth (dette #107, constat 3) :
/// transport HTTP **réel** ([`ReqwestHttpClient`]) + `store` de tokens
/// **chiffré au repos** ([`connector_cipher`] : clé dérivée de
/// `config.encryption_key`) — en `postgres`, la table `connector_tokens` que le
/// worker relit pour le propriétaire de chaque run.
/// Le registre de providers est peuplé au démarrage depuis l'environnement
/// (`AUTOME_OAUTH_PROVIDERS` + `AUTOME_OAUTH_<ID>_*`, #161) — vide en l'absence
/// de config (OAuth alors indisponible) ; une config incomplète échoue (fail-fast).
/// Le transport applique le filtre anti-SSRF : un endpoint OAuth interne (IdP
/// on-prem) doit figurer dans `AUTOME_EGRESS_ALLOWLIST`.
fn build_credential_service(
    store: Arc<dyn ConnectorTokenStore + Send + Sync>,
) -> Result<Arc<dyn CredentialService + Send + Sync>> {
    let oauth = OAuthClient::new(
        OAuthRegistry::from_env()?,
        ReqwestHttpClient::with_policy(EgressPolicy::from_env()),
    );
    Ok(Arc::new(OAuthCredentialService::new(oauth, store)))
}

/// Backends applicatifs assemblés selon le [`PersistenceMode`] : un seul jeu
/// de stores partagé par tous les routers (auth + workflows + runs).
pub struct Backends {
    /// État d'authentification (token + révocations/permissions/audit).
    pub auth: AuthState,
    /// Repository des workflows.
    pub workflows: Arc<dyn WorkflowRepository + Send + Sync>,
    /// Store d'état des runs.
    pub runs: Arc<dyn RunStateStore + Send + Sync>,
    /// Dispatcher de runs (publie sur la file + checkpoint initial).
    pub dispatcher: Arc<dyn RunDispatcher + Send + Sync>,
    /// Service de credentials OAuth (transport HTTP réel + store chiffré).
    pub credentials: Arc<dyn CredentialService + Send + Sync>,
    /// Comptes utilisateurs (signup/login). Écrit dans le même store de
    /// permissions que `auth`, sans quoi un compte créé n'aurait aucun droit.
    pub users: Arc<dyn UserStore + Send + Sync>,
    /// Compteur anti-bruteforce du login (partagé entre répliques en
    /// `postgres`, #182).
    pub login_throttle: Arc<dyn LoginThrottleStore>,
    /// Endpoints des déclencheurs webhook (#202).
    pub webhooks: Arc<dyn WebhookEndpointStore>,
    /// Échéances des déclencheurs planifiés (#227), lues par les workers.
    pub schedules: Arc<dyn TriggerScheduleStore>,
    /// Effacement d'un compte et de ses données (#229) : une transaction en
    /// `postgres`, étape par étape sur les stores en mémoire sinon.
    pub eraser: Arc<dyn AccountEraser>,
    /// Second facteur des comptes (#245), partagé par les comptes et
    /// l'administration.
    pub mfa: Arc<dyn MfaStore>,
}

/// Assemble les backends. En `Postgres`, **un seul pool** est partagé par tous
/// les stores. La `queue` (construite par [`build_queue`] selon `AUTOME_QUEUE`)
/// est injectée — le dispatcher publie dessus.
pub fn build_backends(
    config: &Config,
    mode: PersistenceMode,
    queue: Arc<dyn Queue + Send + Sync>,
) -> Result<Backends> {
    match mode {
        PersistenceMode::Memory => {
            let memory_runs = Arc::new(InMemoryRunStateStore::new());
            let runs: Arc<dyn RunStateStore + Send + Sync> = memory_runs.clone();
            let dispatcher: Arc<dyn RunDispatcher + Send + Sync> =
                Arc::new(QueueRunDispatcher::new(queue, runs.clone()));
            let permissions = Arc::new(InMemoryPermissionStore::new());
            let auth = AuthState::new(
                token_service_from_config(config),
                Arc::new(InMemoryRevocationStore::new()),
                permissions.clone(),
                Arc::new(NoopAuditSink),
            );
            let workflows: Arc<dyn WorkflowRepository + Send + Sync> =
                Arc::new(InMemoryWorkflowRepository::new());
            let tokens: Arc<dyn ConnectorTokenStore + Send + Sync> =
                Arc::new(InMemoryConnectorTokenStore::new(connector_cipher(config)));
            let users = Arc::new(InMemoryUserStore::new(permissions));
            let webhooks: Arc<dyn WebhookEndpointStore> =
                Arc::new(InMemoryWebhookEndpointStore::new());
            let schedules: Arc<dyn TriggerScheduleStore> =
                Arc::new(InMemoryTriggerScheduleStore::new());
            // Audit `Noop` en mémoire : rien à pseudonymiser.
            let eraser = Arc::new(InMemoryAccountEraser::new(
                users.clone(),
                workflows.clone(),
                memory_runs,
                tokens.clone(),
                webhooks.clone(),
                schedules.clone(),
                None,
            ));
            Ok(Backends {
                auth,
                workflows,
                runs,
                dispatcher,
                credentials: build_credential_service(tokens)?,
                users,
                login_throttle: Arc::new(InMemoryLoginThrottle::new()),
                webhooks,
                schedules,
                eraser,
                mfa: Arc::new(InMemoryMfaStore::new()),
            })
        }
        PersistenceMode::Postgres => {
            let pool = connect(&config.database_url)?;
            let auth = AuthState::new(
                token_service_from_config(config),
                Arc::new(PgRevocationStore::new(pool.clone())),
                Arc::new(PgPermissionStore::new(pool.clone())),
                Arc::new(PgAuditSink::new(pool.clone())),
            );
            let runs: Arc<dyn RunStateStore + Send + Sync> =
                Arc::new(PgRunStateStore::new(pool.clone()));
            let dispatcher: Arc<dyn RunDispatcher + Send + Sync> =
                Arc::new(QueueRunDispatcher::new(queue, runs.clone()));
            Ok(Backends {
                auth,
                workflows: Arc::new(PgWorkflowRepository::new(pool.clone())),
                runs,
                dispatcher,
                credentials: build_credential_service(Arc::new(PgConnectorTokenStore::new(
                    pool.clone(),
                    connector_cipher(config),
                )))?,
                login_throttle: Arc::new(PgLoginThrottle::new(pool.clone())),
                webhooks: Arc::new(PgWebhookEndpointStore::new(pool.clone())),
                schedules: Arc::new(PgTriggerScheduleStore::new(pool.clone())),
                eraser: Arc::new(PgAccountEraser::new(pool.clone())),
                mfa: Arc::new(PgMfaStore::new(pool.clone())),
                users: Arc::new(PgUserStore::new(pool)),
            })
        }
    }
}

/// Purge les entrées expirées des stores d'authentification : tentatives de
/// connexion hors fenêtre et révocations de jetons échus. Renvoie
/// `(tentatives, révocations)` supprimées. Appelée périodiquement par l'API ;
/// idempotente, sûre à exécuter sur plusieurs répliques à la fois.
pub async fn purge_expired(
    login_throttle: &dyn LoginThrottleStore,
    revocations: &(dyn RevocationStore + Send + Sync),
    now_unix_seconds: i64,
) -> Result<(usize, usize)> {
    let attempts = login_throttle.gc(now_unix_seconds).await?;
    let revoked = revocations
        .gc(now_unix_seconds.saturating_mul(1_000))
        .await?;
    Ok((attempts, revoked))
}

/// `true` si `AUTOME_MIGRATE` demande d'appliquer les migrations au démarrage
/// (`1`/`true`/`yes`, insensible à la casse).
pub fn migrate_on_boot_requested() -> bool {
    matches!(
        std::env::var("AUTOME_MIGRATE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

/// Applique les migrations sur la base de `config` (mode `postgres`). Renvoie
/// le nombre de migrations appliquées. Échoue si la base est injoignable.
pub async fn migrate(config: &Config) -> Result<usize> {
    let pool = connect(&config.database_url)?;
    run_migrations(&pool).await
}

/// Construit l'[`AuthState`] selon le `mode` :
/// - `Memory` : [`AuthState::from_config`] (in-memory) ;
/// - `Postgres` : pool partagé + stores PG. Le pool n'ouvre pas de connexion à
///   la construction (deadpool connecte à la demande) — les erreurs DB
///   remontent au premier usage.
pub fn build_auth_state(config: &Config, mode: PersistenceMode) -> Result<AuthState> {
    match mode {
        PersistenceMode::Memory => Ok(AuthState::from_config(config)),
        PersistenceMode::Postgres => {
            let pool = connect(&config.database_url)?;
            Ok(AuthState::new(
                token_service_from_config(config),
                Arc::new(PgRevocationStore::new(pool.clone())),
                Arc::new(PgPermissionStore::new(pool.clone())),
                // Audit persistant en base ; passer à NoopAuditSink si l'on
                // veut explicitement désactiver l'audit en prod.
                Arc::new(PgAuditSink::new(pool)),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_mode_builds_in_memory_auth_state() {
        let config = Config::load(|_| None).expect("config dev");
        // Ne doit pas toucher au réseau ni échouer.
        let _state = build_auth_state(&config, PersistenceMode::Memory).unwrap();
    }

    #[test]
    fn postgres_mode_builds_without_connecting() {
        // `build_unchecked` ⇒ la construction n'ouvre pas de connexion ; on peut
        // bâtir l'état avec un DSN qui ne répond pas (les erreurs viendraient au
        // premier usage, pas ici).
        let config = Config::load(|k| match k {
            "APP_PROFILE" => Some("prod".into()),
            "DATABASE_URL" => Some("postgres://u:p@127.0.0.1:1/none".into()),
            "JWT_SECRET" => Some("prod-secret-32-characters-minimum!!".into()),
            "ENCRYPTION_KEY" => Some("prod-encryption-key".into()),
            _ => None,
        })
        .expect("config prod");
        let _state = build_auth_state(&config, PersistenceMode::Postgres).unwrap();
    }

    #[tokio::test]
    async fn purge_expired_cleans_login_attempts_and_revocations() {
        use autome_core::LOGIN_WINDOW_SECONDS;
        let config = Config::load(|_| None).unwrap();
        let queue = Arc::new(autome_core::InMemoryQueue::new());
        let backends = build_backends(&config, PersistenceMode::Memory, queue).unwrap();
        let now = 1_700_000_000;
        backends
            .login_throttle
            .begin_attempt("email:old@example.io", 5, now - LOGIN_WINDOW_SECONDS)
            .await
            .unwrap();
        backends
            .login_throttle
            .begin_attempt("email:live@example.io", 5, now)
            .await
            .unwrap();
        backends
            .auth
            .revocation
            .revoke("jti-expired", (now - 1) * 1_000)
            .await
            .unwrap();
        backends
            .auth
            .revocation
            .revoke("jti-live", (now + 60) * 1_000)
            .await
            .unwrap();

        let purge = || {
            purge_expired(
                backends.login_throttle.as_ref(),
                backends.auth.revocation.as_ref(),
                now,
            )
        };
        assert_eq!(purge().await.unwrap(), (1, 1));
        assert!(backends
            .auth
            .revocation
            .is_revoked("jti-live")
            .await
            .unwrap());
        assert_eq!(purge().await.unwrap(), (0, 0));
    }

    /// Gaté par `TEST_DATABASE_URL` : en `postgres`, `/credentials/*` lit et
    /// révoque les tokens dans la table partagée `connector_tokens` (celle que
    /// le worker relit), plus dans un store propre au process.
    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_backends_use_the_shared_connector_tokens_table() {
        let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
            eprintln!("TEST_DATABASE_URL absent → test PostgreSQL sauté");
            return;
        };
        let config =
            Config::load(move |k| (k == "DATABASE_URL").then(|| url.clone())).expect("config");
        let pool = connect(&config.database_url).unwrap();
        let ddl: String = autome_persistence::MIGRATIONS
            .iter()
            .filter(|(name, _)| {
                [
                    "0011_connector_tokens.sql",
                    "0018_connector_token_invalidation.sql",
                ]
                .contains(name)
            })
            .map(|(_, sql)| *sql)
            .collect();
        pool.get()
            .await
            .unwrap()
            .batch_execute(&format!("DROP TABLE IF EXISTS connector_tokens; {ddl}"))
            .await
            .unwrap();

        // Un autre process (même ENCRYPTION_KEY) a enregistré le token.
        let other_process =
            autome_persistence::build_connector_token_store(&config, PersistenceMode::Postgres)
                .unwrap();
        let tokens = autome_connectors::oauth::TokenSet {
            access_token: "xoxb-api-alice".into(),
            refresh_token: None,
            expires_at_ms: None,
            token_type: "Bearer".into(),
            scope: Some("chat:write".into()),
        };
        other_process.store("api-alice", "slack", &tokens).unwrap();

        let queue = Arc::new(autome_core::InMemoryQueue::new());
        let backends = build_backends(&config, PersistenceMode::Postgres, queue).unwrap();
        let listed = backends.credentials.list_for_user("api-alice").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].provider_id, "slack");
        assert!(backends
            .credentials
            .list_for_user("api-bob")
            .unwrap()
            .is_empty());

        backends.credentials.delete("api-alice", "slack").unwrap();
        assert_eq!(other_process.load("api-alice", "slack").unwrap(), None);
    }
}

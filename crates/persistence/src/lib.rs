//! `autome-persistence` — backends PostgreSQL des traits de stockage de `core`
//! (dette #107, constat 2).
//!
//! Les traits (`RevocationStore`, `PermissionStore`, `AuditSink`, …) sont
//! **asynchrones** ; on utilise le driver async **`tokio-postgres`** derrière
//! un pool **deadpool**, compatible avec le runtime tokio de l'API (un driver
//! bloquant paniquerait — « cannot start a runtime from within a runtime »).
//!
//! Couvert : [`PgRevocationStore`], [`PgPermissionStore`], [`PgAuditSink`],
//! [`PgUserStore`] (stores d'authent/RBAC/audit/comptes),
//! [`PgConnectorTokenStore`] (tokens OAuth des connecteurs, chiffrés) + le
//! [`run_migrations`] runner. Les backends runs/workflows/queue suivent.

pub mod audit;
pub mod connector_tokens;
pub mod migrations;
pub mod permission;
pub mod pool;
pub mod rabbitmq;
pub mod revocation;
pub mod run_state;
pub mod selectors;
pub mod users;
pub mod workflow_repo;

pub use audit::PgAuditSink;
pub use connector_tokens::PgConnectorTokenStore;
pub use migrations::{run_migrations, MIGRATIONS};
pub use permission::PgPermissionStore;
pub use pool::{connect, PgPool};
pub use rabbitmq::RabbitMqQueue;
pub use revocation::PgRevocationStore;
pub use run_state::PgRunStateStore;
pub use selectors::{
    build_connector_token_store, build_queue, build_run_state_store, build_workflow_repository,
    connector_cipher, PersistenceMode, QueueMode,
};
pub use users::PgUserStore;
pub use workflow_repo::PgWorkflowRepository;

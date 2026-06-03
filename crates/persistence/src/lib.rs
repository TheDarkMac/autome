//! `autome-persistence` — backends PostgreSQL des traits de stockage de `core`
//! (dette #107, constat 2).
//!
//! Les traits (`RevocationStore`, `PermissionStore`, `AuditSink`, …) sont
//! **asynchrones** ; on utilise le driver async **`tokio-postgres`** derrière
//! un pool **deadpool**, compatible avec le runtime tokio de l'API (un driver
//! bloquant paniquerait — « cannot start a runtime from within a runtime »).
//!
//! Couvert : [`PgRevocationStore`], [`PgPermissionStore`], [`PgAuditSink`]
//! (stores d'authent/RBAC/audit) + le [`run_migrations`] runner. Les backends
//! runs/workflows/queue suivent.

pub mod audit;
pub mod migrations;
pub mod permission;
pub mod pool;
pub mod revocation;
pub mod run_state;
pub mod workflow_repo;

pub use audit::PgAuditSink;
pub use migrations::{run_migrations, MIGRATIONS};
pub use permission::PgPermissionStore;
pub use pool::{connect, PgPool};
pub use revocation::PgRevocationStore;
pub use run_state::PgRunStateStore;
pub use workflow_repo::PgWorkflowRepository;

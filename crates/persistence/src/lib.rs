//! `autome-persistence` — backends PostgreSQL des traits de stockage de `core`
//! (dette #107, constat 2).
//!
//! Les traits (`RevocationStore`, `PermissionStore`, `AuditSink`, …) sont
//! **synchrones** ; on utilise donc le driver PostgreSQL **bloquant**
//! (`postgres`) derrière un pool [`r2d2`] plutôt qu'un driver async, pour
//! éviter un `block_on` dans les handlers axum.
//!
//! ⚠️ Les appels DB bloquent le thread courant. Sous l'exécuteur async de
//! l'API c'est acceptable au stade actuel (le contrat des traits est sync) ;
//! une bascule vers des traits async — ou un offload via `spawn_blocking` —
//! pourra suivre si le profil de charge l'exige.
//!
//! Couvert : [`PgRevocationStore`], [`PgPermissionStore`], [`PgAuditSink`]
//! (stores d'authent/RBAC/audit). Les backends runs/workflows/queue suivent.

pub mod audit;
pub mod permission;
pub mod pool;
pub mod revocation;

pub use audit::PgAuditSink;
pub use permission::PgPermissionStore;
pub use pool::{connect, PgPool};
pub use revocation::PgRevocationStore;

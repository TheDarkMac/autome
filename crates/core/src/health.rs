//! Vérifications de dépendances pour les sondes de disponibilité (#192).
//!
//! Une sonde *readiness* répond « prêt » quand le process peut servir : ses
//! dépendances (PostgreSQL, broker) sont joignables. À ne **pas** confondre
//! avec la *liveness* (le process est-il bloqué ?) : une liveness qui
//! dépendrait de la base ferait redémarrer en boucle tous les pods pendant une
//! panne de base, sans rien réparer.
//!
//! Les impls concrètes (PostgreSQL, file) et leur exécution bornée en temps
//! vivent dans `autome-persistence`.

use async_trait::async_trait;

use crate::Result;

/// Une dépendance à vérifier.
#[async_trait]
pub trait HealthCheck: Send + Sync {
    /// Nom stable, exposé dans la réponse de la sonde (ex. `postgres`).
    fn name(&self) -> &'static str;

    /// `Ok` si la dépendance répond. Le message d'erreur n'est que journalisé.
    async fn check(&self) -> Result<()>;
}

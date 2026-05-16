//! `autome-worker` — exécuteur de runs.
//!
//! Stateless (ARCHITECTURE.md §2) : consomme un message `{run_id}` depuis la
//! file, charge l'état en base, délègue l'exécution à `engine`. La file
//! (F5.1), le pool de workers (F5.2) et la persistance (F5.4) arrivent au
//! domaine 5 ; ici : squelette F1.3.f.

use autome_core::{Config, NodeId};
use autome_engine::Engine;

/// Traite un run identifié — placeholder.
///
/// Pour l'instant on instancie le moteur et on tente un node : la logique
/// réelle (pull file, checkpoint, retry) relève de F5.*.
fn process_run(run_id: &str) -> autome_core::Result<()> {
    let engine = Engine::new();
    // L'exécution renvoie volontairement une erreur tant que F2.2 n'est pas
    // faite ; on l'absorbe ici pour ne pas faire échouer le squelette.
    let _ = engine.execute_node(&NodeId::new(format!("{run_id}:entry")));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    println!(
        "autome-worker ({:?}) démarré (en attente d'une file — F5.1)",
        config.profile
    );
    process_run("bootstrap")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_run_is_resilient_to_unimplemented_engine() {
        assert!(process_run("run-1").is_ok());
    }
}

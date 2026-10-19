//! Effacement d'un compte : RGPD, droit à l'effacement (art. 17, #229).
//!
//! [`AccountEraser::erase`] retire **en une opération** le compte et tout ce
//! qui lui est rattaché : permissions, workflows de son espace (avec leurs
//! versions, endpoints webhook et échéances planifiées), runs dont il est
//! propriétaire et leurs sorties, tokens de connecteurs. L'impl PostgreSQL le
//! fait dans une seule transaction : un échec n'efface rien.
//!
//! L'audit n'est **pas** supprimé (il trace aussi les actions des autres et
//! l'effacement lui-même) mais **pseudonymisé** : les événements dont
//! l'acteur ou la ressource est l'id ou l'email du compte reçoivent à la
//! place un pseudonyme aléatoire ([`erasure_pseudonym`]), propre à cet
//! effacement. La piste reste cohérente (les événements du compte restent
//! groupés) sans pouvoir être reliée à la personne : rien ne relie le
//! pseudonyme à l'ancien id.
//!
//! Restent hors du store, à la charge de l'appelant : fermer les sessions
//! (version de session) et remettre à zéro les compteurs anti-bruteforce.

use async_trait::async_trait;
use rand::RngCore;

use crate::Result;

/// Préfixe des pseudonymes d'audit d'un compte effacé.
pub const ERASED_PREFIX: &str = "erased:";

/// Pseudonyme aléatoire d'un effacement : `erased:<16 hex>` (64 bits).
pub fn erasure_pseudonym() -> String {
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{ERASED_PREFIX}{hex}")
}

/// Bilan d'un effacement : ce qui a été supprimé ou pseudonymisé.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErasureReport {
    /// Workflows supprimés (versions, webhooks et échéances avec eux).
    pub workflows: usize,
    /// Runs supprimés (sorties des nœuds avec eux).
    pub runs: usize,
    /// Tokens de connecteurs supprimés.
    pub connector_tokens: usize,
    /// Événements d'audit pseudonymisés.
    pub audit_events: usize,
}

/// Effacement atomique d'un compte et de ses données.
#[async_trait]
pub trait AccountEraser: Send + Sync {
    /// Efface le compte `user_id` et tout ce qui lui est rattaché, et
    /// remplace dans l'audit son id et son email par `pseudonym`. `None` si
    /// le compte est inconnu (rien n'est écrit), y compris pour un
    /// identifiant mal formé.
    async fn erase(&self, user_id: &str, pseudonym: &str) -> Result<Option<ErasureReport>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudonyms_are_prefixed_random_and_never_uuid_shaped() {
        let a = erasure_pseudonym();
        let b = erasure_pseudonym();
        assert!(a.starts_with(ERASED_PREFIX));
        assert_eq!(a.len(), ERASED_PREFIX.len() + 16);
        assert!(a[ERASED_PREFIX.len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        // Jamais confondu avec un `claims.sub` (UUID) ni avec un email.
        assert!(!a.contains('-') && !a.contains('@'));
    }
}

//! Refus des mots de passe courants ou compromis (#245).
//!
//! La longueur seule ([`crate::validate_password`]) laisse passer `password`,
//! `12345678` ou `azertyuiop` : les premiers essayés par une pulvérisation de
//! mots de passe, que l'anti-bruteforce par email et par IP ne voit pas
//! quand elle est distribuée. Comme le recommande le NIST (SP 800-63B
//! §5.1.1.2), un mot de passe **choisi** (inscription, changement,
//! réinitialisation, invitation) est comparé :
//!
//! - à une **liste intégrée** des mots de passe les plus fréquents dans les
//!   fuites publiques (ceux d'au moins 8 caractères : les autres sont déjà
//!   refusés par la longueur), comparaison sans casse ;
//! - à des **mots de base** courants (`password`, `azerty`, `soleil`…) une
//!   fois retirés les chiffres et symboles de bord et le *leet speak*
//!   (`P@ssw0rd2024!` ⇒ `password`) ;
//! - au **contexte** : nom du service, email ou sa partie locale ;
//! - à un caractère **répété** (`aaaaaaaa`) ;
//! - à une **liste de l'opérateur** (fichier, un mot de passe par ligne,
//!   cf. [`CommonPasswords::with_list`]).
//!
//! La vérification contre un corpus de fuites (Have I Been Pwned) est une
//! source **distante** facultative, branchée par l'API
//! ([`BreachedPasswordSource`]).

use std::collections::HashSet;

use async_trait::async_trait;

use crate::Result;

/// Mots de passe d'au moins 8 caractères parmi les plus fréquents des fuites
/// publiques (classements annuels, listes francophones), en minuscules.
const COMMON: &[&str] = &[
    "00000000",
    "0000000000",
    "1111111111",
    "11111111",
    "12121212",
    "123123123",
    "12341234",
    "1234512345",
    "12345678",
    "123456789",
    "1234567890",
    "12345678910",
    "123456789a",
    "123456abc",
    "1234abcd",
    "123qweasd",
    "123qweasdzxc",
    "147258369",
    "159753159753",
    "1q2w3e4r",
    "1q2w3e4r5t",
    "1q2w3e4r5t6y",
    "1qaz2wsx",
    "1qaz2wsx3edc",
    "1qazxsw2",
    "20202020",
    "22222222",
    "31415926",
    "55555555",
    "66666666",
    "77777777",
    "87654321",
    "88888888",
    "963852741",
    "987654321",
    "9876543210",
    "99999999",
    "a1b2c3d4",
    "aa123456",
    "aaaaaaaa",
    "abc12345",
    "abc123456",
    "abcd1234",
    "abcdefgh",
    "access14",
    "admin123",
    "admin1234",
    "administrator",
    "asdfasdf",
    "asdfghjk",
    "asdfghjkl",
    "azerty123",
    "azerty1234",
    "azertyui",
    "azertyuiop",
    "babygirl",
    "baseball",
    "basketball",
    "batman123",
    "bienvenue",
    "blink182",
    "butterfly",
    "changeme",
    "charlie1",
    "chelsea1",
    "chocolate",
    "computer",
    "corvette",
    "dearbook",
    "doudou123",
    "dragon123",
    "football",
    "football1",
    "freedom1",
    "iloveyou",
    "iloveyou1",
    "iloveyou2",
    "jennifer",
    "jesus123",
    "jordan23",
    "liverpool",
    "loveme123",
    "marseille",
    "master123",
    "mercedes",
    "michelle",
    "midnight",
    "minecraft",
    "monkey123",
    "motdepasse",
    "mustang1",
    "naruto123",
    "nicole123",
    "nopassword",
    "p@ssw0rd",
    "p@ssword",
    "pa55word",
    "passw0rd",
    "password",
    "password01",
    "password1",
    "password12",
    "password123",
    "password1234",
    "password2",
    "pokemon123",
    "princess",
    "princess1",
    "q1w2e3r4",
    "q1w2e3r4t5",
    "q1w2e3r4t5y6",
    "qazwsxedc",
    "qwer1234",
    "qwerty12",
    "qwerty123",
    "qwerty1234",
    "qwertyui",
    "qwertyuiop",
    "samantha",
    "shadow123",
    "soccer12",
    "starwars",
    "sunshine",
    "sunshine1",
    "superman",
    "superman1",
    "trustno1",
    "unknown1",
    "vanessa1",
    "welcome1",
    "welcome123",
    "whatever",
    "zaq12wsx",
    "zxcvbnm1",
    "zxcvbnm123",
    "zxcvbnmm",
];

/// Mots de base courants, reconnus une fois retirés les chiffres et symboles
/// de bord et le *leet speak* : `Soleil2024!` ⇒ `soleil`.
const BASE_WORDS: &[&str] = &[
    "abc",
    "abcd",
    "abcdef",
    "admin",
    "administrateur",
    "administrator",
    "amour",
    "azerty",
    "azertyuiop",
    "baseball",
    "batman",
    "bienvenue",
    "bonjour",
    "chocolat",
    "chouchou",
    "doudou",
    "dragon",
    "football",
    "iloveyou",
    "jetaime",
    "letmein",
    "login",
    "loulou",
    "love",
    "master",
    "monkey",
    "motdepasse",
    "pass",
    "passe",
    "passwd",
    "password",
    "pokemon",
    "princess",
    "princesse",
    "qwerty",
    "qwertyuiop",
    "root",
    "secret",
    "soleil",
    "starwars",
    "sunshine",
    "superman",
    "test",
    "user",
    "welcome",
];

/// Nom du service, refusé comme base d'un mot de passe.
const SERVICE_NAME: &str = "autome";

/// Une partie locale d'email plus courte n'est pas comparée (`a@x.io` ne doit
/// pas interdire tout mot de passe qui se réduit à `a`).
const MIN_CONTEXT_WORD: usize = 3;

/// Raison du refus d'un mot de passe (journaux, tests ; jamais renvoyée au
/// client, qui reçoit un message unique).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordWeakness {
    /// Figure dans la liste intégrée ou celle de l'opérateur.
    Common,
    /// Se réduit à un mot de base courant (`P@ssw0rd2024!`).
    CommonBase,
    /// Se réduit au nom du service, à l'email ou à sa partie locale.
    Context,
    /// Un seul caractère répété.
    Repetitive,
    /// Présent dans un corpus de fuites (source distante).
    Breached,
}

/// Message renvoyé au client pour tout mot de passe refusé par ce contrôle.
pub const COMPROMISED_PASSWORD_MESSAGE: &str =
    "mot de passe trop courant ou divulgué dans une fuite de données : choisissez-en un autre";

/// Listes locales des mots de passe refusés.
#[derive(Debug, Clone)]
pub struct CommonPasswords {
    exact: HashSet<String>,
    bases: HashSet<&'static str>,
}

impl Default for CommonPasswords {
    fn default() -> Self {
        Self::builtin()
    }
}

impl CommonPasswords {
    /// Listes intégrées seulement.
    pub fn builtin() -> Self {
        Self {
            exact: COMMON.iter().map(|p| (*p).to_string()).collect(),
            bases: BASE_WORDS.iter().copied().collect(),
        }
    }

    /// Ajoute la liste de l'opérateur : un mot de passe par ligne, lignes
    /// vides et commentaires (`#`) ignorés, comparaison sans casse.
    pub fn with_list(mut self, contents: &str) -> Self {
        self.exact.extend(
            contents
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_lowercase),
        );
        self
    }

    /// Nombre de mots de passe refusés à l'identique (intégrés + opérateur).
    pub fn len(&self) -> usize {
        self.exact.len()
    }

    /// `true` si aucune entrée (jamais le cas des listes intégrées).
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }

    /// Raison du refus de `password` pour le compte `email` (normalisé), ou
    /// `None` s'il passe les contrôles locaux.
    pub fn weakness(&self, password: &str, email: Option<&str>) -> Option<PasswordWeakness> {
        let lower = password.to_lowercase();
        if self.exact.contains(&lower) {
            return Some(PasswordWeakness::Common);
        }
        let mut chars = lower.chars();
        if let Some(first) = chars.next() {
            if chars.all(|c| c == first) {
                return Some(PasswordWeakness::Repetitive);
            }
        }
        let base = base_word(&lower);
        if base.is_empty() {
            // Que des chiffres et symboles : la liste exacte a déjà jugé.
            return None;
        }
        if self.bases.contains(base.as_str()) {
            return Some(PasswordWeakness::CommonBase);
        }
        if context_words(email)
            .iter()
            .any(|w| *w == base || *w == lower)
        {
            return Some(PasswordWeakness::Context);
        }
        None
    }
}

/// Mot de base de `lower` : *leet speak* ramené aux lettres, puis chiffres,
/// symboles et espaces de bord retirés (`p@ssw0rd2024!` ⇒ `password`).
fn base_word(lower: &str) -> String {
    let unleet: String = lower
        .chars()
        .map(|c| match c {
            '0' => 'o',
            '1' | '!' => 'i',
            '3' => 'e',
            '4' | '@' => 'a',
            '5' | '$' => 's',
            '7' => 't',
            other => other,
        })
        .collect();
    // Le leet ne s'applique qu'à l'intérieur : `password2024!` garde ses
    // chiffres de fin, retirés ensuite comme bord.
    let core_start = lower
        .char_indices()
        .find(|(_, c)| c.is_alphabetic())
        .map(|(i, _)| i);
    let core_end = lower
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphabetic())
        .map(|(i, c)| i + c.len_utf8());
    let (Some(start), Some(end)) = (core_start, core_end) else {
        return String::new();
    };
    // Même découpage dans `unleet` : la substitution conserve les longueurs
    // (caractères ASCII remplacés par des caractères ASCII).
    unleet[start..end].to_string()
}

/// Mots du contexte refusés comme base : nom du service, email complet,
/// partie locale et ses morceaux (`jean.dupont` ⇒ `jean`, `dupont`).
fn context_words(email: Option<&str>) -> Vec<String> {
    let mut words = vec![SERVICE_NAME.to_string()];
    let Some(email) = email else {
        return words;
    };
    words.push(email.to_lowercase());
    if let Some((local, _)) = email.split_once('@') {
        let local = local.to_lowercase();
        words.extend(
            local
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.chars().count() >= MIN_CONTEXT_WORD)
                .map(base_word),
        );
        words.push(base_word(&local));
    }
    words.retain(|w| w.chars().count() >= MIN_CONTEXT_WORD);
    words
}

/// Corpus distant de mots de passe divulgués (ex. Have I Been Pwned en
/// k-anonymat) : `true` si `password` y figure. Une erreur signale une
/// source injoignable, à l'appelant de décider (l'API applique alors les
/// seules listes locales).
#[async_trait]
pub trait BreachedPasswordSource: Send + Sync {
    /// `true` si `password` figure dans le corpus.
    async fn is_breached(&self, password: &str) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(password: &str) -> Option<PasswordWeakness> {
        CommonPasswords::builtin().weakness(password, Some("jean.dupont@example.io"))
    }

    #[test]
    fn builtin_entries_are_lowercase_and_long_enough_to_matter() {
        // Une entrée de moins de 8 caractères ne servirait à rien (refusée
        // par la longueur) ; une majuscule ne serait jamais comparée.
        for p in COMMON {
            assert!(p.chars().count() >= 8, "{p} : trop court pour la liste");
            assert_eq!(*p, p.to_lowercase(), "{p} : minuscules attendues");
        }
        for w in BASE_WORDS {
            assert_eq!(*w, w.to_lowercase());
            assert_eq!(base_word(w), *w, "{w} : doit être sa propre base");
        }
    }

    #[test]
    fn common_passwords_are_refused_whatever_the_case() {
        assert_eq!(check("password"), Some(PasswordWeakness::Common));
        assert_eq!(check("PassWord"), Some(PasswordWeakness::Common));
        assert_eq!(check("12345678"), Some(PasswordWeakness::Common));
        assert_eq!(check("AZERTYUIOP"), Some(PasswordWeakness::Common));
        assert_eq!(check("1q2w3e4r"), Some(PasswordWeakness::Common));
    }

    #[test]
    fn decorated_common_words_are_refused() {
        assert_eq!(check("P@ssw0rd2024!"), Some(PasswordWeakness::CommonBase));
        assert_eq!(check("Soleil2024!"), Some(PasswordWeakness::CommonBase));
        assert_eq!(check("!!azerty!!"), Some(PasswordWeakness::CommonBase));
        assert_eq!(check("Bonjour123"), Some(PasswordWeakness::CommonBase));
        assert_eq!(check("Admin2026"), Some(PasswordWeakness::CommonBase));
    }

    #[test]
    fn context_words_are_refused() {
        assert_eq!(check("Autome2026!"), Some(PasswordWeakness::Context));
        assert_eq!(check("dupont1984"), Some(PasswordWeakness::Context));
        assert_eq!(check("Jean.Dupont99"), Some(PasswordWeakness::Context));
        assert_eq!(
            check("jean.dupont@example.io"),
            Some(PasswordWeakness::Context)
        );
        // Sans email, seul le nom du service est un contexte.
        let list = CommonPasswords::builtin();
        assert_eq!(list.weakness("dupont1984", None), None);
        assert_eq!(
            list.weakness("autome2026", None),
            Some(PasswordWeakness::Context)
        );
        // Partie locale trop courte : pas un contexte.
        assert_eq!(list.weakness("jo123456!", Some("jo@example.io")), None);
    }

    #[test]
    fn repeated_characters_are_refused() {
        assert_eq!(check("zzzzzzzzzz"), Some(PasswordWeakness::Repetitive));
        assert_eq!(check("€€€€€€€€"), Some(PasswordWeakness::Repetitive));
    }

    #[test]
    fn strong_or_unusual_passwords_pass() {
        for ok in [
            "correct horse battery staple",
            "Tr0ub4dor&3-mais-plus-long",
            "vélo-rouge-sous-la-pluie",
            "73 moutons et 1 chèvre",
            "48151623",
            "mot de passe avec émoji 🔐",
            // Contient un mot courant sans s'y réduire.
            "mon password préféré est long",
        ] {
            assert_eq!(check(ok), None, "{ok} ne devrait pas être refusé");
        }
    }

    #[test]
    fn operator_list_is_added_case_insensitively() {
        let list = CommonPasswords::builtin()
            .with_list("# fuite interne\n\nAcmeCorp2024\n  société-secrète  \n");
        assert_eq!(list.len(), COMMON.len() + 2);
        assert_eq!(
            list.weakness("acmecorp2024", None),
            Some(PasswordWeakness::Common)
        );
        assert_eq!(
            list.weakness("SOCIÉTÉ-SECRÈTE", None),
            Some(PasswordWeakness::Common)
        );
        assert_eq!(list.weakness("# fuite interne", None), None);
    }

    #[test]
    fn base_word_strips_edges_and_leet() {
        assert_eq!(base_word("p@ssw0rd2024!"), "password");
        assert_eq!(base_word("2024"), "");
        assert_eq!(base_word("été2024"), "été");
        // Bords retirés avant le leet : `4` de tête n'est pas un `a`.
        assert_eq!(base_word("4zert7"), "zert");
    }
}

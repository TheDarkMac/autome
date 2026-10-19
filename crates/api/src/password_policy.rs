//! Refus des mots de passe courants ou compromis à leur choix (#245).
//!
//! Appliqué partout où un mot de passe est **choisi** : inscription,
//! changement, réinitialisation, acceptation d'une invitation — jamais à la
//! connexion (un compte existant ne doit pas être verrouillé par une liste
//! mise à jour). Deux niveaux :
//!
//! - **local**, toujours actif : listes intégrées, règles de contexte et
//!   fichier de l'opérateur (`AUTOME_PASSWORD_BLOCKLIST_FILE`), cf.
//!   [`CommonPasswords`] ;
//! - **distant**, sur option (`AUTOME_PWNED_PASSWORDS=hibp`) : corpus Have I
//!   Been Pwned en **k-anonymat** ([`HibpRange`]). Seuls les 5 premiers
//!   caractères hexadécimaux du SHA-1 du mot de passe quittent le serveur ;
//!   la réponse est **complétée de faux suffixes** (`Add-Padding`), si bien
//!   que sa taille ne révèle rien non plus. `AUTOME_PWNED_PASSWORDS_URL`
//!   désigne un miroir interne (même API `GET /range/{préfixe}`).
//!
//! Source distante en panne ou trop lente ([`HIBP_TIMEOUT`]) : le mot de
//! passe est jugé par les seules listes locales et un avertissement est
//! journalisé (jamais le mot de passe ni son préfixe) — l'inscription ne doit
//! pas dépendre de la disponibilité d'un tiers.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use autome_core::{BreachedPasswordSource, CommonPasswords, Error, PasswordWeakness};
use sha1::{Digest, Sha1};

/// URL publique de l'API *Pwned Passwords* (sans clé d'API).
pub const HIBP_DEFAULT_URL: &str = "https://api.pwnedpasswords.com";
/// Délai maximal d'une interrogation du corpus distant.
pub const HIBP_TIMEOUT: Duration = Duration::from_secs(3);
/// Taille maximale acceptée d'une réponse (une plage complétée fait ~40 Kio).
const HIBP_MAX_BODY: usize = 1024 * 1024;

/// Contrôle des mots de passe choisis.
#[derive(Clone)]
pub struct PasswordPolicy {
    local: Arc<CommonPasswords>,
    breached: Option<Arc<dyn BreachedPasswordSource>>,
}

impl std::fmt::Debug for PasswordPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordPolicy")
            .field("local_entries", &self.local.len())
            .field("breached_source", &self.breached.is_some())
            .finish()
    }
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self::new(CommonPasswords::builtin())
    }
}

impl PasswordPolicy {
    /// Listes locales seulement.
    pub fn new(local: CommonPasswords) -> Self {
        Self {
            local: Arc::new(local),
            breached: None,
        }
    }

    /// Ajoute une source distante de mots de passe divulgués.
    pub fn with_breached_source(mut self, source: Arc<dyn BreachedPasswordSource>) -> Self {
        self.breached = Some(source);
        self
    }

    /// `true` si une source distante est branchée.
    pub fn checks_breaches(&self) -> bool {
        self.breached.is_some()
    }

    /// Nombre de mots de passe des listes locales (journal de démarrage).
    pub fn local_entries(&self) -> usize {
        self.local.len()
    }

    /// Depuis l'environnement : `AUTOME_PASSWORD_BLOCKLIST_FILE`,
    /// `AUTOME_PWNED_PASSWORDS` (`off` par défaut, ou `hibp`),
    /// `AUTOME_PWNED_PASSWORDS_URL`. Configuration invalide ou fichier
    /// illisible ⇒ refus de démarrer.
    pub fn from_env() -> Result<Self, String> {
        Self::from_source(
            |name| std::env::var(name).ok(),
            |path| std::fs::read_to_string(path).map_err(|e| e.to_string()),
        )
    }

    /// Comme [`from_env`](Self::from_env), variables lues via `get` et
    /// fichier via `read` (tests).
    pub fn from_source(
        get: impl Fn(&str) -> Option<String>,
        read: impl Fn(&str) -> Result<String, String>,
    ) -> Result<Self, String> {
        let set = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        let mut local = CommonPasswords::builtin();
        if let Some(path) = set("AUTOME_PASSWORD_BLOCKLIST_FILE") {
            let contents = read(path.trim())
                .map_err(|e| format!("AUTOME_PASSWORD_BLOCKLIST_FILE ({path}) illisible : {e}"))?;
            local = local.with_list(&contents);
        }
        let policy = Self::new(local);
        match set("AUTOME_PWNED_PASSWORDS")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("off") => Ok(policy),
            Some("hibp") => {
                let url = set("AUTOME_PWNED_PASSWORDS_URL")
                    .unwrap_or_else(|| HIBP_DEFAULT_URL.to_string());
                Ok(policy.with_breached_source(Arc::new(HibpRange::new(url.trim())?)))
            }
            Some(other) => Err(format!(
                "AUTOME_PWNED_PASSWORDS : `off` ou `hibp` attendu, obtenu `{other}`"
            )),
        }
    }

    /// Raison du refus de `password` choisi pour le compte `email`
    /// (normalisé), ou `None` s'il est accepté.
    pub async fn weakness(&self, password: &str, email: Option<&str>) -> Option<PasswordWeakness> {
        if let Some(weakness) = self.local.weakness(password, email) {
            return Some(weakness);
        }
        let source = self.breached.as_ref()?;
        match source.is_breached(password).await {
            Ok(true) => Some(PasswordWeakness::Breached),
            Ok(false) => None,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "corpus des mots de passe divulgués injoignable : listes locales seules"
                );
                None
            }
        }
    }
}

/// Corpus Have I Been Pwned interrogé en k-anonymat (`GET /range/{5 hex}`).
#[derive(Debug, Clone)]
pub struct HibpRange {
    client: reqwest::Client,
    base_url: String,
}

impl HibpRange {
    /// Client vers `base_url` (`https://api.pwnedpasswords.com` ou un miroir
    /// interne), délai [`HIBP_TIMEOUT`].
    pub fn new(base_url: &str) -> Result<Self, String> {
        let base_url = base_url.trim_end_matches('/');
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err(format!(
                "AUTOME_PWNED_PASSWORDS_URL : URL http(s) attendue, obtenu `{base_url}`"
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(HIBP_TIMEOUT)
            .user_agent(concat!("autome/", env!("CARGO_PKG_VERSION")))
            // Aucune redirection : le préfixe ne part que vers l'hôte configuré.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("client HTTP du corpus de mots de passe : {e}"))?;
        Ok(Self {
            client,
            base_url: base_url.to_string(),
        })
    }
}

/// SHA-1 de `password` en hexadécimal majuscule (format de l'API), découpé en
/// `(préfixe de 5, suffixe de 35)`.
fn sha1_range(password: &str) -> (String, String) {
    let digest = Sha1::digest(password.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02X}")).collect();
    let (prefix, suffix) = hex.split_at(5);
    (prefix.to_string(), suffix.to_string())
}

/// `true` si la réponse de plage `body` (`SUFFIXE:COMPTE` par ligne) contient
/// `suffix` avec un compte non nul (les lignes de remplissage valent 0).
fn range_contains(body: &str, suffix: &str) -> bool {
    body.lines().any(|line| {
        line.trim().split_once(':').is_some_and(|(s, count)| {
            s.eq_ignore_ascii_case(suffix) && count.trim().parse::<u64>().is_ok_and(|c| c > 0)
        })
    })
}

#[async_trait]
impl BreachedPasswordSource for HibpRange {
    async fn is_breached(&self, password: &str) -> autome_core::Result<bool> {
        let (prefix, suffix) = sha1_range(password);
        let err = |what: &str, e: &dyn std::fmt::Display| {
            Error::Invalid(format!("pwned passwords: {what}: {e}"))
        };
        let response = self
            .client
            .get(format!("{}/range/{prefix}", self.base_url))
            .header("Add-Padding", "true")
            .send()
            .await
            .map_err(|e| err("request", &e))?;
        if !response.status().is_success() {
            return Err(err("status", &response.status()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > HIBP_MAX_BODY as u64)
        {
            return Err(err("body", &"response too large"));
        }
        let body = response.bytes().await.map_err(|e| err("body", &e))?;
        if body.len() > HIBP_MAX_BODY {
            return Err(err("body", &"response too large"));
        }
        Ok(range_contains(&String::from_utf8_lossy(&body), &suffix))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use axum::Router;

    use super::*;

    #[test]
    fn sha1_range_matches_the_documented_example() {
        // Exemple de la documentation de l'API : SHA-1("P@ssw0rd").
        let (prefix, suffix) = sha1_range("P@ssw0rd");
        assert_eq!(prefix, "21BD1");
        assert_eq!(suffix, "2DC183F740EE76F27B78EB39C8AD972A757");
    }

    #[test]
    fn range_lines_with_a_zero_count_are_padding() {
        let body = "0018A45C4D1DEF81644B54AB7F969B88D65:1\r\n\
                    2DC183F740EE76F27B78EB39C8AD972A757:52579\r\n\
                    00D4F6E8FA6EECAD2A3AA415EEC418D38EC:0\r\n";
        assert!(range_contains(body, "2DC183F740EE76F27B78EB39C8AD972A757"));
        assert!(range_contains(body, "2dc183f740ee76f27b78eb39c8ad972a757"));
        assert!(!range_contains(body, "00D4F6E8FA6EECAD2A3AA415EEC418D38EC"));
        assert!(!range_contains(body, "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"));
        assert!(!range_contains("", "2DC183F740EE76F27B78EB39C8AD972A757"));
    }

    /// Faux corpus local : enregistre les préfixes demandés et l'en-tête de
    /// remplissage, sert une plage qui contient `breached`.
    async fn fake_corpus(breached: &str, status: u16) -> (String, Arc<Mutex<Vec<(String, bool)>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (_, suffix) = sha1_range(breached);
        let log = seen.clone();
        let app = Router::new().route(
            "/range/:prefix",
            get(move |Path(prefix): Path<String>, headers: HeaderMap| {
                let log = log.clone();
                let suffix = suffix.clone();
                async move {
                    let padded = headers.get("add-padding").is_some_and(|v| v == "true");
                    log.lock().unwrap().push((prefix, padded));
                    let status = axum::http::StatusCode::from_u16(status).unwrap();
                    (
                        status,
                        format!("{suffix}:3\r\nABCDEF0123456789ABCDEF0123456789ABC:0"),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn only_the_hash_prefix_leaves_the_server_with_padding() {
        let (url, seen) = fake_corpus("une phrase divulguée", 200).await;
        let hibp = HibpRange::new(&format!("{url}/")).unwrap();
        assert!(hibp.is_breached("une phrase divulguée").await.unwrap());
        assert!(!hibp.is_breached("une autre phrase, inédite").await.unwrap());
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        for (prefix, padded) in &seen {
            assert_eq!(prefix.len(), 5, "seuls 5 caractères du SHA-1 sortent");
            assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(padded, "remplissage demandé");
        }
        assert_eq!(seen[0].0, sha1_range("une phrase divulguée").0);
    }

    #[tokio::test]
    async fn the_policy_combines_local_lists_and_the_remote_corpus() {
        let (url, _) = fake_corpus("une phrase divulguée", 200).await;
        let policy =
            PasswordPolicy::default().with_breached_source(Arc::new(HibpRange::new(&url).unwrap()));
        assert!(policy.checks_breaches());
        assert_eq!(
            policy.weakness("password123", None).await,
            Some(PasswordWeakness::Common)
        );
        assert_eq!(
            policy.weakness("une phrase divulguée", None).await,
            Some(PasswordWeakness::Breached)
        );
        assert_eq!(
            policy.weakness("une autre phrase, inédite", None).await,
            None
        );
    }

    /// Source distante toujours en échec, qui compte ses appels.
    struct Down(AtomicUsize);

    #[async_trait]
    impl BreachedPasswordSource for Down {
        async fn is_breached(&self, _: &str) -> autome_core::Result<bool> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(Error::Invalid("down".into()))
        }
    }

    #[tokio::test]
    async fn an_unreachable_corpus_falls_back_to_the_local_lists() {
        let down = Arc::new(Down(AtomicUsize::new(0)));
        let policy = PasswordPolicy::default().with_breached_source(down.clone());
        assert_eq!(
            policy.weakness("une autre phrase, inédite", None).await,
            None
        );
        // Refusé localement : la source distante n'est même pas interrogée.
        assert_eq!(
            policy.weakness("azertyuiop", None).await,
            Some(PasswordWeakness::Common)
        );
        assert_eq!(down.0.load(Ordering::SeqCst), 1);

        let (url, _) = fake_corpus("x", 503).await;
        let hibp = HibpRange::new(&url).unwrap();
        assert!(hibp.is_breached("x").await.is_err(), "503 = indisponible");
    }

    #[test]
    fn configuration_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        let no_file = |_: &str| -> Result<String, String> { Err("absent".into()) };

        let default = PasswordPolicy::from_source(env(&[]), no_file).unwrap();
        assert!(!default.checks_breaches());
        let off = PasswordPolicy::from_source(env(&[("AUTOME_PWNED_PASSWORDS", "off")]), no_file)
            .unwrap();
        assert!(!off.checks_breaches());
        let hibp =
            PasswordPolicy::from_source(env(&[("AUTOME_PWNED_PASSWORDS", " HIBP ")]), no_file)
                .unwrap();
        assert!(hibp.checks_breaches());
        let err = PasswordPolicy::from_source(env(&[("AUTOME_PWNED_PASSWORDS", "on")]), no_file)
            .unwrap_err();
        assert!(err.contains("AUTOME_PWNED_PASSWORDS"), "{err}");
        let err = PasswordPolicy::from_source(
            env(&[
                ("AUTOME_PWNED_PASSWORDS", "hibp"),
                ("AUTOME_PWNED_PASSWORDS_URL", "ftp://mirror"),
            ]),
            no_file,
        )
        .unwrap_err();
        assert!(err.contains("AUTOME_PWNED_PASSWORDS_URL"), "{err}");

        // Fichier de l'opérateur : ajouté aux listes ; illisible ⇒ erreur.
        let with_file = PasswordPolicy::from_source(
            env(&[("AUTOME_PASSWORD_BLOCKLIST_FILE", "/etc/autome/blocklist")]),
            |path| {
                assert_eq!(path, "/etc/autome/blocklist");
                Ok("AcmeCorp2024\n".into())
            },
        )
        .unwrap();
        assert_eq!(
            with_file.local_entries(),
            CommonPasswords::builtin().len() + 1
        );
        let err = PasswordPolicy::from_source(
            env(&[("AUTOME_PASSWORD_BLOCKLIST_FILE", "/nope")]),
            no_file,
        )
        .unwrap_err();
        assert!(err.contains("/nope"), "{err}");
    }
}

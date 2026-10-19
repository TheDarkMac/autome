//! Garde-fou SSRF des requêtes HTTP sortantes (backlog post-audit — P0).
//!
//! L'action `http_request` prend son URL dans les `params` du node, donc
//! **contrôlée par l'utilisateur**, et le worker l'exécute depuis le réseau
//! du serveur. Sans filtre, un workflow peut viser `169.254.169.254`
//! (métadonnées cloud → identifiants d'instance), `localhost`, ou tout
//! service interne du cluster.
//!
//! [`EgressPolicy`] n'autorise que des destinations **publiques** :
//! - schémas `http`/`https` uniquement ([`EgressPolicy::check_url`]) ;
//! - une IP littérale doit être publique ([`is_public_ip`]) ;
//! - un nom d'hôte est résolu puis **toutes** ses adresses doivent être
//!   publiques ([`EgressPolicy::check_resolved`]). Ce contrôle est fait par le
//!   résolveur DNS du client (`reqwest_client.rs`) : la connexion part vers
//!   les adresses validées, ce qui neutralise le *DNS rebinding* (un second
//!   lookup ne peut pas substituer une IP interne) ;
//! - chaque redirection est revérifiée.
//!
//! Exception explicite : les hôtes listés par l'opérateur dans
//! [`ALLOWLIST_ENV`] (intranet légitime, IdP interne…) ne sont pas filtrés.
//!
//! Les proxys sortants de l'environnement ([`PROXY_ENV_VARS`]) sont ignorés
//! par le client filtré (#233) : le proxy résoudrait lui-même l'hôte cible,
//! hors de portée du résolveur. [`proxy_vars_set`] permet de le signaler au
//! démarrage.

use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::Url;

/// Variable d'environnement : hôtes internes autorisés malgré le filtre,
/// séparés par virgules/espaces (ex. `keycloak.internal,10.0.0.12`).
pub const ALLOWLIST_ENV: &str = "AUTOME_EGRESS_ALLOWLIST";

/// Variables de proxy que les clients HTTP suivent d'habitude et que le
/// client filtré ignore.
pub const PROXY_ENV_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// Variables de [`PROXY_ENV_VARS`] définies (non vides) dans `src`.
pub fn proxy_vars_set(src: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    PROXY_ENV_VARS
        .into_iter()
        .filter(|k| src(k).is_some_and(|v| !v.trim().is_empty()))
        .collect()
}

/// Refus d'une destination sortante.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressError(pub String);

impl fmt::Display for EgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "egress refusé : {}", self.0)
    }
}

impl std::error::Error for EgressError {}

/// Politique de sortie réseau. `Default` = stricte (aucune exception).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Hôtes (noms ou IP littérales) exemptés, en minuscules.
    allowlist: HashSet<String>,
}

impl EgressPolicy {
    /// Politique stricte : seules les destinations publiques passent.
    pub fn strict() -> Self {
        Self::default()
    }

    /// Politique stricte + hôtes exemptés.
    pub fn with_allowed_hosts<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            allowlist: hosts
                .into_iter()
                .map(|h| normalize_host(h.as_ref()))
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }

    /// Lit l'allowlist depuis une source clé → valeur (injectable en test).
    pub fn from_source(src: impl Fn(&str) -> Option<String>) -> Self {
        let raw = src(ALLOWLIST_ENV).unwrap_or_default();
        Self::with_allowed_hosts(raw.split(|c: char| c == ',' || c.is_whitespace()))
    }

    /// Lit l'allowlist depuis l'environnement ([`ALLOWLIST_ENV`]).
    pub fn from_env() -> Self {
        Self::from_source(|k| std::env::var(k).ok())
    }

    /// `true` si `host` est exempté par l'opérateur.
    pub fn is_allowlisted(&self, host: &str) -> bool {
        self.allowlist.contains(&normalize_host(host))
    }

    /// Contrôle statique d'une URL (avant toute résolution) : schéma, hôte
    /// présent, et IP littérale publique. Un nom d'hôte est contrôlé à la
    /// résolution ([`Self::check_resolved`]).
    pub fn check_url(&self, url: &Url) -> Result<(), EgressError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(EgressError(format!(
                "schéma `{}` interdit (http/https uniquement)",
                url.scheme()
            )));
        }
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| EgressError("URL sans hôte".into()))?;
        // L'URL normalise déjà les IPv4 exotiques (`2130706433`, `0x7f.1`) en
        // forme pointée ; une IPv6 littérale est entre crochets.
        let Ok(ip) = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        else {
            return Ok(());
        };
        if self.is_allowlisted(&ip.to_string()) || is_public_ip(ip) {
            Ok(())
        } else {
            Err(EgressError(format!("adresse non publique {ip}")))
        }
    }

    /// Contrôle des adresses résolues pour `host`. Toutes doivent être
    /// publiques : un nom qui mêle adresses publiques et internes est refusé
    /// en bloc plutôt que filtré (signe de rebinding ou de mauvaise config).
    pub fn check_resolved(
        &self,
        host: &str,
        addrs: Vec<SocketAddr>,
    ) -> Result<Vec<SocketAddr>, EgressError> {
        if addrs.is_empty() {
            return Err(EgressError(format!(
                "`{host}` ne résout vers aucune adresse"
            )));
        }
        if self.is_allowlisted(host) {
            return Ok(addrs);
        }
        if let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
            return Err(EgressError(format!(
                "`{host}` résout vers l'adresse non publique {}",
                bad.ip()
            )));
        }
        Ok(addrs)
    }
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// `true` si `ip` est routable sur Internet : ni loopback, ni privée, ni
/// link-local (métadonnées cloud), ni réservée/documentation/multicast.
/// Les adresses IPv6 qui encapsulent une IPv4 (mappée, compatible, NAT64,
/// 6to4) sont jugées sur l'IPv4 encapsulée.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0                                  // 0.0.0.0/8 « ce réseau »
        || (a == 100 && (b & 0xc0) == 64)          // 100.64.0.0/10 CGNAT
        || (a == 192 && b == 0 && c == 0)          // 192.0.0.0/24 IETF
        || (a == 198 && (b & 0xfe) == 18)          // 198.18.0.0/15 benchmark
        || a >= 240) // 240.0.0.0/4 réservé
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    // IPv4 mappée (::ffff:a.b.c.d) ou compatible (::a.b.c.d, dont :: et ::1).
    if let Some(v4) = ip.to_ipv4() {
        return is_public_v4(v4);
    }
    // NAT64 (64:ff9b::/96) : IPv4 dans les 32 bits de poids faible.
    if seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0] {
        return is_public_v4(embedded_v4(seg[6], seg[7]));
    }
    // 6to4 (2002::/16) : IPv4 dans les bits 16..48.
    if seg[0] == 0x2002 {
        return is_public_v4(embedded_v4(seg[1], seg[2]));
    }
    !(ip.is_multicast()                            // ff00::/8
        || (seg[0] & 0xfe00) == 0xfc00             // fc00::/7 ULA
        || (seg[0] & 0xffc0) == 0xfe80             // fe80::/10 link-local
        || (seg[0] & 0xffc0) == 0xfec0             // fec0::/10 site-local
        || (seg[0] == 0x2001 && seg[1] == 0x0db8)  // 2001:db8::/32 doc
        || (seg[0] == 0x2001 && seg[1] == 0)       // 2001::/32 Teredo
        || (seg[0] == 0x0100 && seg[1..4] == [0, 0, 0])) // 100::/64 discard
}

fn embedded_v4(hi: u16, lo: u16) -> Ipv4Addr {
    let [a, b] = hi.to_be_bytes();
    let [c, d] = lo.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn sock(s: &str) -> SocketAddr {
        SocketAddr::new(ip(s), 443)
    }

    #[test]
    fn public_addresses_are_allowed() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2a00:1450:4007:80e::200e",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(is_public_ip(ip(s)), "{s} devrait être publique");
        }
    }

    #[test]
    fn internal_and_reserved_addresses_are_rejected() {
        for s in [
            "127.0.0.1",
            "127.255.255.254",
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254", // métadonnées AWS/GCP/Azure
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "100::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::a9fe:a9fe", // NAT64 → 169.254.169.254
            "2002:a9fe:a9fe::1",  // 6to4 → 169.254.169.254
            "2002:7f00:1::1",     // 6to4 → 127.0.0.1
        ] {
            assert!(!is_public_ip(ip(s)), "{s} devrait être refusée");
        }
    }

    #[test]
    fn neighbours_of_reserved_ranges_stay_public() {
        for s in [
            "100.63.255.255",
            "100.128.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "172.32.0.1",
        ] {
            assert!(is_public_ip(ip(s)), "{s} devrait être publique");
        }
    }

    #[test]
    fn check_url_enforces_scheme_and_literal_ips() {
        let p = EgressPolicy::strict();
        assert!(p.check_url(&url("https://example.com/x")).is_ok());
        assert!(p.check_url(&url("http://8.8.8.8/")).is_ok());
        // Un nom d'hôte est jugé à la résolution, pas ici.
        assert!(p.check_url(&url("http://localhost:8080/")).is_ok());

        for bad in [
            "file:///etc/passwd",
            "gopher://example.com/",
            "ftp://example.com/",
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:5432/",
            "http://[::1]/",
            "http://[::ffff:10.0.0.1]/",
            "http://evil.com@169.254.169.254/",
            "http://2130706433/", // 127.0.0.1 en décimal (normalisé par l'URL)
            "http://0x7f.1/",     // 127.0.0.1 en hexa
        ] {
            let err = p.check_url(&url(bad)).unwrap_err();
            assert!(
                err.to_string().starts_with("egress refusé"),
                "{bad} : {err}"
            );
        }
    }

    #[test]
    fn check_resolved_rejects_any_internal_address() {
        let p = EgressPolicy::strict();
        assert_eq!(
            p.check_resolved("ok.example", vec![sock("93.184.216.34")]),
            Ok(vec![sock("93.184.216.34")])
        );
        let err = p
            .check_resolved(
                "rebind.example",
                vec![sock("93.184.216.34"), sock("10.0.0.5")],
            )
            .unwrap_err();
        assert!(err.0.contains("10.0.0.5"));
        assert!(p
            .check_resolved("localhost", vec![sock("127.0.0.1")])
            .is_err());
        assert!(p.check_resolved("void.example", vec![]).is_err());
    }

    #[test]
    fn allowlist_exempts_listed_hosts_only() {
        let p = EgressPolicy::from_source(|k| {
            (k == ALLOWLIST_ENV).then(|| " Keycloak.Internal, 10.0.0.12 [::1] localhost.".into())
        });
        assert!(p.is_allowlisted("keycloak.internal"));
        assert!(p.is_allowlisted("KEYCLOAK.internal."));
        assert!(p.is_allowlisted("::1"));
        assert!(p.is_allowlisted("localhost"));
        assert!(p
            .check_resolved("keycloak.internal", vec![sock("10.1.2.3")])
            .is_ok());
        assert!(p.check_url(&url("http://10.0.0.12:8080/token")).is_ok());
        assert!(p.check_url(&url("http://[::1]/")).is_ok());
        // Non listés : toujours filtrés.
        assert!(p.check_url(&url("http://10.0.0.13/")).is_err());
        assert!(p
            .check_resolved("other.internal", vec![sock("10.1.2.3")])
            .is_err());
    }

    #[test]
    fn empty_allowlist_is_strict() {
        assert_eq!(EgressPolicy::from_source(|_| None), EgressPolicy::strict());
        assert_eq!(
            EgressPolicy::from_source(|_| Some(" , ".into())),
            EgressPolicy::strict()
        );
    }

    #[test]
    fn proxy_vars_set_lists_non_empty_proxy_variables() {
        assert!(proxy_vars_set(|_| None).is_empty());
        let set = proxy_vars_set(|k| match k {
            "HTTPS_PROXY" => Some("http://proxy.internal:3128".into()),
            "all_proxy" => Some("socks5://10.0.0.9:1080".into()),
            "HTTP_PROXY" => Some("  ".into()),
            _ => None,
        });
        assert_eq!(set, ["HTTPS_PROXY", "all_proxy"]);
    }
}

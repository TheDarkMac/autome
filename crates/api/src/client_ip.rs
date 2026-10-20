//! Adresse IP cliente et clé de compteur pseudonymisée (#190).
//!
//! **IP réelle.** Derrière l'ingress ou un load balancer, le pair TCP est le
//! proxy, le même pour tous les clients. On lit alors `X-Forwarded-For`, mais
//! **seulement** si le pair est un proxy de confiance
//! (`AUTOME_TRUSTED_PROXIES`, liste de CIDR) : sinon n'importe quel client
//! forgerait l'en-tête pour changer d'« IP » à chaque requête. L'adresse
//! retenue est la plus à droite de la chaîne qui ne soit pas un proxy de
//! confiance — les entrées plus à gauche sont fournies par le client.
//!
//! **Données personnelles.** Une adresse IP est une donnée personnelle (RGPD).
//! Elle ne sert qu'à l'anti-abus et n'est jamais stockée ni journalisée : le
//! compteur est indexé par un HMAC-SHA256 à clé secrète (dérivée
//! d'`ENCRYPTION_KEY`) de l'adresse — un simple hachage se renverserait en
//! énumérant les 2³² IPv4. Les IPv6 sont comptées par préfixe `/64`, souvent
//! attribué en entier à un seul abonné.

use std::net::IpAddr;

use autome_core::Config;
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use ipnet::IpNet;
use sha2::{Digest, Sha256};

/// Proxys dont on accepte l'en-tête `X-Forwarded-For`.
#[derive(Debug, Clone, Default)]
pub struct ClientIpPolicy {
    trusted: Vec<IpNet>,
}

impl ClientIpPolicy {
    /// Lit `AUTOME_TRUSTED_PROXIES` (CIDR ou adresses, séparés par des
    /// virgules). Absente ou vide : aucun proxy de confiance, l'adresse du pair
    /// fait foi. Une entrée invalide est une erreur de démarrage.
    pub fn from_env() -> Result<Self, String> {
        Self::parse(&std::env::var("AUTOME_TRUSTED_PROXIES").unwrap_or_default())
    }

    /// Analyse une liste `10.0.0.0/8, 192.168.1.10, fd00::/8`.
    pub fn parse(list: &str) -> Result<Self, String> {
        let trusted = list
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                entry
                    .parse::<IpNet>()
                    .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
                    .map_err(|_| format!("AUTOME_TRUSTED_PROXIES : entrée invalide `{entry}`"))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { trusted })
    }

    /// `true` si `peer` est un proxy de confiance : ses en-têtes de
    /// transfert (`X-Forwarded-For`, `traceparent` — #249) sont repris.
    pub fn trusts(&self, peer: IpAddr) -> bool {
        self.is_trusted(peer)
    }

    fn is_trusted(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.trusted.iter().any(|net| net.contains(&ip))
    }

    /// Adresse du client pour une requête reçue du pair `peer`.
    pub fn client_ip(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let peer = canonical(peer);
        if !self.is_trusted(peer) {
            return peer;
        }
        // Chaîne complète, toutes occurrences de l'en-tête dans l'ordre.
        let chain: Vec<&str> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .collect();
        let mut client = peer;
        for entry in chain.iter().rev() {
            match entry.parse::<IpAddr>() {
                Ok(ip) if self.is_trusted(ip) => client = canonical(ip),
                Ok(ip) => return canonical(ip),
                // Entrée illisible : on s'arrête au dernier proxy de confiance
                // plutôt que de croire ce qui est plus à gauche.
                Err(_) => return client,
            }
        }
        client
    }
}

/// IPv4 encapsulée dans une IPv6 (`::ffff:a.b.c.d`) ⇒ IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// Clés de compteur pseudonymisées des adresses IP.
#[derive(Clone)]
pub struct IpKeyer {
    key: [u8; 32],
}

impl std::fmt::Debug for IpKeyer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IpKeyer(<redacted>)")
    }
}

impl IpKeyer {
    /// Clé HMAC dérivée de `secret` (séparation de domaine : elle diffère de
    /// toute autre clé tirée du même secret).
    pub fn new(secret: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"autome/throttle-ip/v1\0");
        hasher.update(secret);
        Self {
            key: hasher.finalize().into(),
        }
    }

    /// Clé dérivée d'`ENCRYPTION_KEY` : identique sur toutes les répliques.
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.encryption_key.expose().as_bytes())
    }

    /// Clé du compteur `scope` (ex. `login-ip`) pour l'adresse `ip`.
    pub fn key(&self, scope: &str, ip: IpAddr) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepte toute longueur");
        match canonical(ip) {
            IpAddr::V4(v4) => mac.update(&v4.octets()),
            // Préfixe /64 : un abonné IPv6 dispose de tout le bloc.
            IpAddr::V6(v6) => mac.update(&v6.octets()[..8]),
        }
        let digest = mac.finalize().into_bytes();
        format!("{scope}:{}", hex::encode(digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(xff: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in xff {
            h.append("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn without_trusted_proxies_the_header_is_ignored() {
        let policy = ClientIpPolicy::default();
        let h = headers(&["1.2.3.4"]);
        assert_eq!(policy.client_ip(ip("203.0.113.7"), &h), ip("203.0.113.7"));
    }

    #[test]
    fn an_untrusted_peer_cannot_spoof_its_address() {
        let policy = ClientIpPolicy::parse("10.0.0.0/8").unwrap();
        let h = headers(&["1.2.3.4"]);
        assert_eq!(policy.client_ip(ip("203.0.113.7"), &h), ip("203.0.113.7"));
    }

    #[test]
    fn behind_trusted_proxies_the_rightmost_untrusted_hop_wins() {
        let policy = ClientIpPolicy::parse("10.0.0.0/8, 192.168.1.10").unwrap();
        // Le client a forgé `6.6.6.6` ; l'ingress (10.0.0.5) a ajouté la vraie
        // adresse, puis un proxy interne (192.168.1.10).
        let h = headers(&["6.6.6.6, 198.51.100.9", "192.168.1.10"]);
        assert_eq!(policy.client_ip(ip("10.0.0.5"), &h), ip("198.51.100.9"));
    }

    #[test]
    fn missing_or_unreadable_header_falls_back_to_the_last_trusted_hop() {
        let policy = ClientIpPolicy::parse("10.0.0.0/8").unwrap();
        assert_eq!(
            policy.client_ip(ip("10.0.0.5"), &HeaderMap::new()),
            ip("10.0.0.5")
        );
        let h = headers(&["198.51.100.9, garbage, 10.0.0.9"]);
        assert_eq!(policy.client_ip(ip("10.0.0.5"), &h), ip("10.0.0.9"));
    }

    #[test]
    fn ipv4_mapped_addresses_are_normalized() {
        let policy = ClientIpPolicy::parse("10.0.0.0/8").unwrap();
        let h = headers(&["::ffff:198.51.100.9"]);
        assert_eq!(
            policy.client_ip(ip("::ffff:10.0.0.5"), &h),
            ip("198.51.100.9")
        );
    }

    #[test]
    fn invalid_entries_are_rejected() {
        assert!(ClientIpPolicy::parse("10.0.0.0/8, nope").is_err());
        assert!(ClientIpPolicy::parse(" , ").unwrap().trusted.is_empty());
        assert!(ClientIpPolicy::parse("fd00::/8,::1").is_ok());
    }

    #[test]
    fn keys_are_scoped_secret_and_never_contain_the_address() {
        let keyer = IpKeyer::new(b"secret-a");
        let k = keyer.key("login-ip", ip("198.51.100.9"));
        assert!(k.starts_with("login-ip:"));
        assert_eq!(k.len(), "login-ip:".len() + 64);
        assert!(!k.contains("198.51"));
        assert_eq!(k, keyer.key("login-ip", ip("198.51.100.9")));
        assert_ne!(k, keyer.key("login-ip", ip("198.51.100.10")));
        assert_ne!(
            k,
            IpKeyer::new(b"secret-b").key("login-ip", ip("198.51.100.9"))
        );
        assert_eq!(
            keyer.key("login-ip", ip("::ffff:198.51.100.9")),
            k,
            "IPv4 encapsulée = même compteur"
        );
    }

    #[test]
    fn ipv6_is_counted_per_64_prefix() {
        let keyer = IpKeyer::new(b"s");
        assert_eq!(
            keyer.key("login-ip", ip("2001:db8:1:2::1")),
            keyer.key("login-ip", ip("2001:db8:1:2:ffff::9"))
        );
        assert_ne!(
            keyer.key("login-ip", ip("2001:db8:1:2::1")),
            keyer.key("login-ip", ip("2001:db8:1:3::1"))
        );
    }

    #[test]
    fn debug_does_not_leak_the_key() {
        assert_eq!(format!("{:?}", IpKeyer::new(b"s")), "IpKeyer(<redacted>)");
    }
}

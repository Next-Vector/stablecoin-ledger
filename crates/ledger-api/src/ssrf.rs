//! SSRF protection for webhook receivers (FR-57). In production a receiver must be a public
//! address: a URL is checked when a subscription is saved, and again at every delivery, where
//! names go through [`PublicResolver`], so a name that starts resolving to an internal address
//! later (DNS rebinding) is still refused before any connection is made.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// How long saving a subscription waits for its host to resolve.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Which receivers webhooks may reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UrlPolicy {
    /// Production: public addresses only.
    #[default]
    PublicOnly,
    /// Development and tests: anywhere, this machine included.
    AllowPrivate,
}

/// Private, loopback, link-local and other addresses that are not on the public internet.
pub fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => internal_v4(ip),
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map_or_else(|| internal_v6(ip), internal_v4),
    }
}

fn internal_v4(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    let this_network = first == 0;
    let shared = first == 100 && (64..128).contains(&second);
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || this_network
        || shared
}

fn internal_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
}

/// A receiver address the policy refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub IpAddr);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "address {} is not allowed: receivers must be public, not private, loopback or link-local",
            self.0
        )
    }
}

impl std::error::Error for Refused {}

/// The address in `url` when its host is an IP literal (in any form the URL standard accepts,
/// such as `2130706433` for 127.0.0.1).
pub(crate) fn literal_ip(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Refuses a URL whose host is an internal IP literal; names are left to [`PublicResolver`].
pub(crate) fn check_literal(url: &Url) -> Result<(), Refused> {
    match literal_ip(url) {
        Some(ip) if is_internal(ip) => Err(Refused(ip)),
        _ => Ok(()),
    }
}

/// Checks a receiver URL before a subscription is saved: its host must resolve, and to public
/// addresses only. Returns why not.
pub async fn check_receiver(url: &str) -> Result<(), String> {
    let url = Url::parse(url).map_err(|_| "is not a valid URL".to_owned())?;
    if let Some(ip) = literal_ip(&url) {
        return check_ip(ip);
    }
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .ok()
        .and_then(Result::ok)
        .map(|addrs| addrs.collect::<Vec<_>>())
        .filter(|addrs| !addrs.is_empty())
        .ok_or_else(|| format!("host {host} does not resolve"))?;
    addrs.iter().try_for_each(|addr| check_ip(addr.ip()))
}

fn check_ip(ip: IpAddr) -> Result<(), String> {
    if is_internal(ip) {
        Err(Refused(ip).to_string())
    } else {
        Ok(())
    }
}

/// Resolves receiver names for the dispatcher and fails when any address is internal, so the
/// connection is never opened.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if let Some(addr) = addrs.iter().find(|addr| is_internal(addr.ip())) {
                return Err(Refused(addr.ip()).into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_addresses_are_recognised() {
        for internal in [
            "127.0.0.1",
            "127.255.0.9",
            "10.0.0.1",
            "172.16.5.4",
            "172.31.255.255",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_internal(internal.parse().unwrap()), "{internal}");
        }
        for public in [
            "93.184.215.14",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_internal(public.parse().unwrap()), "{public}");
        }
    }

    #[test]
    fn ip_literals_are_read_in_every_url_form() {
        let ip = |url: &str| literal_ip(&Url::parse(url).unwrap());
        assert_eq!(
            ip("http://127.0.0.1:8080/a"),
            Some("127.0.0.1".parse().unwrap())
        );
        assert_eq!(ip("http://2130706433/"), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(ip("http://0x7f.1/"), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(ip("http://[::1]/"), Some("::1".parse().unwrap()));
        assert_eq!(ip("https://merchant.example/"), None);
        assert!(check_literal(&Url::parse("http://[fe80::1]/").unwrap()).is_err());
        assert!(check_literal(&Url::parse("https://93.184.215.14/").unwrap()).is_ok());
        assert!(check_literal(&Url::parse("https://localhost/").unwrap()).is_ok());
    }

    #[tokio::test]
    async fn the_resolver_refuses_names_of_internal_addresses() {
        let error = PublicResolver
            .resolve("localhost".parse().unwrap())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("not allowed"), "{error}");
    }
}

//! Which sites and addresses are local, domain lists, and DNS that refuses to send a public
//! site name to this PC or the home network (DNS rebinding).

use crate::sysproxy;
use hyper_util::client::legacy::connect::dns::{GaiResolver, Name};
use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
};

/// `host` is on `list` (one of its sites or a subdomain of one), in any letter case (lists
/// are saved lowercase, but apps may send `Bank.COM`). No allocation: it runs on every
/// connection, for exclusions, Windows' own servers, Zen's never-decrypted sites, and more.
pub fn on_domain_list<S: AsRef<str>>(list: &[S], host: &str) -> bool {
    let host = host.trim_end_matches('.').as_bytes();
    list.iter().any(|site| {
        let site = site.as_ref().as_bytes();
        host.len() >= site.len()
            && host[host.len() - site.len()..].eq_ignore_ascii_case(site)
            && (host.len() == site.len() || host[host.len() - site.len() - 1] == b'.')
    })
}

/// This PC and home-network devices (routers, docks, NAS, smart home, Tailscale). Their
/// certificates are usually self-made, and there are no ads there, so they're never decrypted.
/// Runs several times per connection, so letter case is ignored without making a copy.
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(ip);
    }
    let ends_with = |suffix: &str| host.len() >= suffix.len() && host.as_bytes()[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes());
    host.eq_ignore_ascii_case("localhost") || sysproxy::LOCAL_SUFFIXES.iter().any(|s| ends_with(s)) || !host.contains('.')
}

pub fn is_private_ip(ip: IpAddr) -> bool {
    // `::ffff:127.0.0.1` is the IPv4 address wearing IPv6 clothes
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            // 0.0.0.0/8 and 100.64.0.0/10 (Tailscale, carrier NAT) too, like the PAC script
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_broadcast() || v4.is_multicast() || a == 0 || (a == 100 && (64..128).contains(&b))
        }
        // Multicast and the old site-local range (fec0::/10) too
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local() || v6.is_unicast_link_local() || v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfec0
        }
    }
}

/// DNS for the proxy's own connections. A public site name that resolves to this PC or the
/// home network is refused: otherwise a web page could flip its DNS to 127.0.0.1 or the router
/// and read local services through the proxy, where the browser can't see the address to stop it.
/// Local names (`nas.local`, `router`, IP addresses) resolve as usual.
#[derive(Clone)]
pub struct PublicOnly(GaiResolver);

impl Default for PublicOnly {
    fn default() -> Self {
        Self(GaiResolver::new())
    }
}

impl tower_service::Service<Name> for PublicOnly {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error    = io::Error;
    type Future   = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let local   = is_private_host(name.as_str());
        let resolve = self.0.call(name);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = resolve.await?.filter(|a| local || !is_private_ip(a.ip())).collect();
            if addrs.is_empty() {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "site name points at a local address"));
            }
            Ok(addrs.into_iter())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn public_names_cannot_resolve_to_local_addresses() {
        use std::str::FromStr;
        use tower_service::Service;

        let mut dns = PublicOnly::default();
        // A local name may point at this PC
        assert!(dns.call(Name::from_str("localhost").unwrap()).await.is_ok());
        // A public name that resolves to 127.0.0.1 (needs internet; skipped when DNS is unreachable)
        match dns.call(Name::from_str("127.0.0.1.nip.io").unwrap()).await {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
            Err(e) => eprintln!("skipped, DNS unavailable: {e}"),
            Ok(addrs) => panic!("resolved to {:?}", addrs.collect::<Vec<_>>()),
        }
    }

    #[test]
    fn home_network_hosts_are_never_decrypted() {
        for host in ["192.168.1.10", "10.0.0.5", "172.20.1.1", "169.254.1.1", "127.0.0.1", "[::1]", "[fd00::1]", "[::ffff:127.0.0.1]", "100.100.1.2", "0.0.0.0", "nas.local", "router", "dock.lan", "app.localhost", "NAS.Local", "LOCALHOST", "Printer.HOME.ARPA"] {
            assert!(is_private_host(host), "{host}");
        }
        for host in ["reddit.com", "8.8.8.8", "172.32.0.1", "100.128.0.1", "[2606:4700::1]", "[::ffff:8.8.8.8]", "local.com", "Reddit.COM"] {
            assert!(!is_private_host(host), "{host}");
        }
    }

    #[test]
    fn zen_sites_are_never_decrypted() {
        let zen = sysproxy::never_decrypted();
        assert!(on_domain_list(zen, "github.com") && on_domain_list(zen, "gist.github.com") && on_domain_list(zen, "Login.Live.com"));
        assert!(!on_domain_list(zen, "notgithub.com"), "look-alikes aren't");
        assert!(!on_domain_list(zen, "x.com") && !on_domain_list(zen, "www.youtube.com"));
    }

    #[test]
    fn domain_lists_match_subdomains_only() {
        let list = vec!["bank.com".to_string()];
        assert!(on_domain_list(&list, "bank.com"));
        assert!(on_domain_list(&list, "www.bank.com"));
        assert!(!on_domain_list(&list, "notbank.com"));
        assert!(!on_domain_list(&list, "bank.com.evil.com"));
        assert!(on_domain_list(&list, "WWW.Bank.COM"), "apps may send any letter case");
        assert!(on_domain_list(&list, "bank.com."));
        assert!(!on_domain_list(&list, "ank.com"));

        // Windows Update and Store servers are never decrypted
        for host in ["fe3cr.delivery.mp.microsoft.com", "tlu.dl.delivery.mp.microsoft.com", "sls.update.microsoft.com", "download.windowsupdate.com", "login.live.com"] {
            assert!(on_domain_list(&sysproxy::SYSTEM_HOSTS, host), "{host}");
        }
        assert!(!on_domain_list(&sysproxy::SYSTEM_HOSTS, "www.microsoft.com"));
    }
}

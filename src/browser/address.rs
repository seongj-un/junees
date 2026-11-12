//! Which addresses the browser may load: http and https only, and no
//! private, loopback or other non-public address unless the user allows
//! them (`JUNEES_BROWSER_ALLOW_PRIVATE`). Every request of every page goes
//! through [`Policy::check`] (the `Fetch` domain pauses each one), so a
//! page cannot reach the machine's own services or the local network
//! (SSRF) through a link, an image, a redirect or a script either.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use reqwest::Url;

/// What the browser may load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// Private, loopback and link-local addresses too.
    pub allow_private: bool,
    /// The addresses of the proxies the browser connects through: a page
    /// that came through one reports the proxy's address, not the site's,
    /// so [`Policy::check_remote`] does not judge those.
    pub proxies: Vec<IpAddr>,
    /// Tests: this one address counts as public, so a page on a loopback
    /// test server loads while the rest of loopback stays refused.
    #[cfg(test)]
    pub public_for_test: Option<IpAddr>,
    /// Tests: [`Policy::check`] resolves these names to these addresses,
    /// while the browser is told others (DNS rebinding as the browser sees
    /// it) or a proxy resolves them.
    #[cfg(test)]
    pub resolve_for_test: Vec<(String, IpAddr)>,
}

impl Policy {
    pub fn new(allow_private: bool) -> Self {
        Self {
            allow_private,
            proxies: Vec::new(),
            #[cfg(test)]
            public_for_test: None,
            #[cfg(test)]
            resolve_for_test: Vec::new(),
        }
    }

    /// Why `url` may not be loaded, judged by its text alone: a scheme
    /// other than http/https, or a host that is a non-public address
    /// literal. Names are resolved by [`Policy::check`].
    pub fn check_url(&self, url: &str) -> Result<Url, String> {
        let parsed = Url::parse(url).map_err(|_| format!("`{}` is not a URL", cut(url)))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "only http and https pages may be opened, not {}: URLs",
                parsed.scheme()
            ));
        }
        let Some(host) = parsed.host_str() else {
            return Err(format!("`{}` has no host", cut(url)));
        };
        if let Some(ip) = ip_literal(host) {
            self.check_ip(ip)?;
        } else if !self.allow_private && is_local_name(host) {
            return Err(private(host));
        }
        Ok(parsed)
    }

    /// [`Policy::check_url`], then every address the host resolves to.
    /// Chrome resolves the name again when it connects, so a name whose
    /// answer changes in between (DNS rebinding) is not caught here; the
    /// address a page then came from is checked with
    /// [`Policy::check_remote`].
    pub async fn check(&self, url: &str) -> Result<(), String> {
        let parsed = self.check_url(url)?;
        let host = parsed.host_str().unwrap_or_default();
        if self.allow_private || ip_literal(host).is_some() {
            return Ok(());
        }
        #[cfg(test)]
        if let Some((_, ip)) = self.resolve_for_test.iter().find(|(name, _)| name == host) {
            return self.check_ip(*ip);
        }
        let port = parsed.port_or_known_default().unwrap_or(80);
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .map_err(|err| format!("cannot resolve {host}: {err}"))?;
        let mut any = false;
        for address in addresses {
            any = true;
            self.check_ip(address.ip()).map_err(|_| {
                format!(
                    "{host} resolves to {}, a private or local address",
                    address.ip()
                )
            })?;
        }
        if !any {
            return Err(format!("cannot resolve {host}"));
        }
        Ok(())
    }

    /// The address `remote` (a response's `remoteIPAddress`) when the
    /// browser may not show what came from it: a private or local address
    /// although the name passed [`Policy::check`] (its answer changed in
    /// between). No address (a cached or service worker response) passes,
    /// and so does a proxy's address: the proxy resolved the name, so the
    /// site's address is not known here.
    pub fn check_remote(&self, remote: &str) -> Result<(), IpAddr> {
        match ip_literal(remote) {
            Some(ip)
                if self
                    .proxies
                    .iter()
                    .any(|proxy| proxy.to_canonical() == ip.to_canonical()) =>
            {
                Ok(())
            }
            Some(ip) => self.check_ip(ip).map_err(|_| ip),
            None => Ok(()),
        }
    }

    fn check_ip(&self, ip: IpAddr) -> Result<(), String> {
        #[cfg(test)]
        if self.public_for_test == Some(ip) {
            return Ok(());
        }
        if self.allow_private || is_public(ip) {
            Ok(())
        } else {
            Err(private(&ip.to_string()))
        }
    }
}

/// The addresses of the proxy hosts `hosts` (names resolved here, as the
/// browser will; a name that does not resolve in 5 seconds adds none).
pub async fn proxy_addresses(hosts: &[String]) -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    for host in hosts {
        if let Some(ip) = ip_literal(host) {
            addresses.push(ip);
        } else if let Ok(Ok(found)) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host.as_str(), 0)),
        )
        .await
        {
            addresses.extend(found.map(|address| address.ip()));
        }
    }
    addresses
}

fn private(host: &str) -> String {
    format!(
        "{host} is a private or local address; the browser opens those only with \
         browser_allow_private (JUNEES_BROWSER_ALLOW_PRIVATE) on"
    )
}

/// `url` shortened for a message.
fn cut(url: &str) -> String {
    let mut text: String = url.chars().take(200).collect();
    if text.len() < url.len() {
        text.push_str("...");
    }
    crate::prompt_text::escape_hidden(&text)
}

/// The address a URL host names directly (`[::1]` with brackets).
fn ip_literal(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Names that always mean this machine or the local network.
fn is_local_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
}

/// Whether `ip` is an address on the public internet: not loopback,
/// private (RFC 1918, unique local), link-local, shared (CGNAT),
/// unspecified, broadcast, multicast, documentation, benchmarking or
/// reserved. IPv6 addresses that carry an IPv4 address (mapped,
/// compatible, NAT64, 6to4) count as that address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..128).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (18..20).contains(&b))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let embedded = |high: usize| {
        let [x, y] = [segments[high], segments[high + 1]];
        Ipv4Addr::new((x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8)
    };
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    // :: and ::1, and the deprecated IPv4-compatible ::a.b.c.d.
    if segments[..6] == [0; 6] {
        return false;
    }
    // NAT64 (64:ff9b::/96) carries the IPv4 address in the last 32 bits.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_public_v4(embedded(6));
    }
    // 6to4 (2002::/16) carries the IPv4 address in the next 32 bits.
    if segments[0] == 0x2002 {
        return is_public_v4(embedded(1));
    }
    let first = segments[0];
    !((first & 0xfe00) == 0xfc00 // unique local
        || (first & 0xffc0) == 0xfe80 // link-local
        || (first & 0xffc0) == 0xfec0 // site-local
        || (first & 0xff00) == 0xff00 // multicast
        || (first == 0x2001 && segments[1] == 0x0db8) // documentation
        || (first == 0x0100 && segments[1..4] == [0, 0, 0])) // discard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_count_as_public() {
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "127.0.0.1",
            "127.255.0.9",
            "100.64.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "192.0.2.10",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:c0a8:0101::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "100.128.0.1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2606:4700:4700::1111",
            "2002:0808:0808::1",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn urls_other_than_http_and_https_are_refused() {
        let policy = Policy::new(true);
        for url in [
            "file:///etc/passwd",
            "FILE:///etc/hostname",
            "chrome://version",
            "chrome-extension://abc/x.js",
            "javascript:alert(1)",
            "data:text/html,<p>x</p>",
            "about:blank",
            "ftp://example.com/",
            "view-source:http://example.com/",
        ] {
            let err = policy.check_url(url).unwrap_err();
            assert!(err.contains("only http and https"), "{url}: {err}");
        }
        assert!(policy.check_url("not a url").is_err());
        assert!(policy.check_url("https://example.com/a?b").is_ok());
        assert!(policy.check_url("http://127.0.0.1:8080/").is_ok());
    }

    #[tokio::test]
    async fn private_addresses_need_the_setting() {
        let policy = Policy::new(false);
        for url in [
            "http://127.0.0.1/",
            "http://[::1]:8080/",
            "http://10.0.0.5/",
            "http://169.254.169.254/latest/meta-data/",
            "http://localhost:3000/",
            "http://app.localhost/",
            "http://printer.local/",
            "http://[::ffff:192.168.0.1]/",
            "http://0x7f000001/",
            "http://2130706433/",
        ] {
            let err = policy.check(url).await.unwrap_err();
            assert!(err.contains("private or local address"), "{url}: {err}");
            assert!(err.contains("JUNEES_BROWSER_ALLOW_PRIVATE"), "{url}: {err}");
        }
        assert!(policy.check_url("http://93.184.216.34/").is_ok());
        let allowed = Policy::new(true);
        for url in [
            "http://127.0.0.1/",
            "http://localhost:3000/",
            "http://[::1]/",
        ] {
            assert!(allowed.check(url).await.is_ok(), "{url}");
        }
    }

    #[test]
    fn the_address_a_page_came_from_is_checked() {
        let policy = Policy::new(false);
        for remote in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.2",
            "[::1]",
            "::1",
            "[fd00::1]",
        ] {
            assert!(policy.check_remote(remote).is_err(), "{remote}");
        }
        for remote in ["93.184.216.34", "[2606:4700:4700::1111]", ""] {
            assert!(policy.check_remote(remote).is_ok(), "{remote}");
        }
        assert!(Policy::new(true).check_remote("127.0.0.1").is_ok());
        // A proxy's address: the proxy resolved the name.
        let proxied = Policy {
            proxies: vec!["10.0.0.2".parse().unwrap(), "::1".parse().unwrap()],
            ..Policy::new(false)
        };
        for remote in ["10.0.0.2", "::ffff:10.0.0.2", "[::1]"] {
            assert!(proxied.check_remote(remote).is_ok(), "{remote}");
        }
        assert!(proxied.check_remote("10.0.0.3").is_err());
        assert!(proxied.check_remote("127.0.0.1").is_err());
    }
}

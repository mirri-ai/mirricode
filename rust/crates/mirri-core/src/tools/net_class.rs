//! Shared network address classifier (ADR-010 decision 2).
//!
//! This module is the authoritative address-grading implementation for
//! private-network fetch gating (see `docs/specs/approval-architecture.md`,
//! ADR-010 decision 2). Both the permission policy and the fetch provider
//! consume it, so the single rule list below is the only source of truth:
//!
//! - **Metadata** (permanent denial): the cloud metadata endpoints
//!   IPv4 `169.254.169.254` and IPv6 `fd00:ec2::254`.
//! - **Askable** (approval flow): loopback, RFC1918, IPv4 link-local
//!   (excluding the metadata endpoint), CGNAT, IPv4 unspecified
//!   (`0.0.0.0/8`), IPv6 `::1` / `fe80::/10` / `fc00::/7`, and the
//!   hostnames `localhost` / `*.localhost` (case-insensitive).
//! - **Public**: everything else.
//!
//! IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are normalized to IPv4
//! before classification — `::ffff:169.254.169.254` must grade as Metadata,
//! not Public (fixes the historical bypass).
//!
//! NAT64 (`64:ff9b::/96`) is deliberately graded Public for now; tightening
//! it is deferred to its own ADR.
//!
//! Multi-address hosts take the strictest grade across all resolved
//! addresses (Metadata > Askable > Public).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use url::Url;

/// The security grade of a network address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetClass {
    /// Public internet.
    Public,
    /// Private / loopback / link-local / CGNAT — requires approval.
    Askable,
    /// Cloud metadata endpoint — permanently denied.
    Metadata,
}

impl NetClass {
    /// The strictest of two grades: `Metadata > Askable > Public`.
    pub fn strictest(self, other: NetClass) -> NetClass {
        use NetClass::*;
        match (self, other) {
            (Metadata, _) | (_, Metadata) => Metadata,
            (Askable, _) | (_, Askable) => Askable,
            (Public, Public) => Public,
        }
    }
}

/// How long a cached DNS classification stays valid.
const DNS_CACHE_TTL: Duration = Duration::from_secs(30);
/// Cache capacity — when full, the map is cleared and rebuilt wholesale.
const DNS_CACHE_CAPACITY: usize = 256;
/// Ceiling for the async DNS lookup.
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// A cached DNS result: when it was stored, and the classification it
/// carried.
type DnsEntry = (Instant, (NetClass, Vec<IpAddr>));

struct DnsCache {
    entries: Mutex<HashMap<String, DnsEntry>>,
}

impl DnsCache {
    fn get(&self, host: &str) -> Option<(NetClass, Vec<IpAddr>)> {
        let entries = self.entries.lock().ok()?;
        let (stored_at, value) = entries.get(&host.to_lowercase())?;
        // Expired entries are treated as absent; a full-table rebuild
        // happens once the capacity ceiling is hit on put.
        (stored_at.elapsed() <= DNS_CACHE_TTL).then(|| value.clone())
    }

    fn put(&self, host: &str, value: (NetClass, Vec<IpAddr>)) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if entries.len() >= DNS_CACHE_CAPACITY {
            entries.clear();
        }
        entries.insert(host.to_lowercase(), (Instant::now(), value));
    }
}

fn dns_cache() -> &'static DnsCache {
    static CACHE: OnceLock<DnsCache> = OnceLock::new();
    CACHE.get_or_init(|| DnsCache {
        entries: Mutex::new(HashMap::new()),
    })
}

/// Normalize an IPv4-mapped IPv6 address to its IPv4 form; other addresses
/// pass through unchanged.
fn normalize_mapped(ip: &IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(*ip),
        IpAddr::V4(_) => *ip,
    }
}

/// Human-readable segment name for an address, used in approval prompts and
/// logs.
pub fn segment_name(ip: &IpAddr) -> &'static str {
    let ip = normalize_mapped(ip);
    match ip {
        IpAddr::V4(v4) => segment_name_v4(&v4),
        IpAddr::V6(v6) => segment_name_v6(&v6),
    }
}

fn segment_name_v4(ip: &Ipv4Addr) -> &'static str {
    let o = ip.octets();
    if ip.is_loopback() {
        "loopback"
    } else if ip.is_private() {
        "RFC1918"
    } else if o == [169, 254, 169, 254] {
        "cloud metadata"
    } else if ip.is_link_local() {
        "link-local"
    } else if is_cgnat_v4(ip) {
        "CGNAT"
    } else if o[0] == 0 {
        "unspecified"
    } else {
        "public"
    }
}

fn segment_name_v6(ip: &Ipv6Addr) -> &'static str {
    if ip.is_loopback() {
        "loopback"
    } else if ip.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254] {
        "cloud metadata"
    } else if is_ula_v6(ip) {
        "IPv6 ULA"
    } else if is_link_local_v6(ip) {
        "IPv6 link-local"
    } else {
        "public"
    }
}

/// CGNAT shared address space: `100.64.0.0/10`.
fn is_cgnat_v4(ip: &Ipv4Addr) -> bool {
    ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1])
}

/// IPv6 unique local addresses: `fc00::/7`.
fn is_ula_v6(ip: &Ipv6Addr) -> bool {
    ip.segments()[0] & 0xfe00 == 0xfc00
}

/// IPv6 link-local addresses: `fe80::/10`.
fn is_link_local_v6(ip: &Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

/// Classify a single IP address against the ADR-010 rule list.
pub fn classify_ip(ip: &IpAddr) -> NetClass {
    let ip = normalize_mapped(ip);
    match ip {
        IpAddr::V4(v4) => classify_v4(&v4),
        IpAddr::V6(v6) => classify_v6(&v6),
    }
}

fn classify_v4(ip: &Ipv4Addr) -> NetClass {
    let o = ip.octets();
    if o == [169, 254, 169, 254] {
        NetClass::Metadata
    } else if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || is_cgnat_v4(ip)
        || o[0] == 0
    {
        NetClass::Askable
    } else {
        NetClass::Public
    }
}

fn classify_v6(ip: &Ipv6Addr) -> NetClass {
    if ip.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254] {
        NetClass::Metadata
    } else if ip.is_loopback() || is_link_local_v6(ip) || is_ula_v6(ip) {
        NetClass::Askable
    } else {
        NetClass::Public
    }
}

/// Strip surrounding brackets from a bracketed IPv6 literal (`[::1]` →
/// `::1`); other inputs pass through unchanged.
fn strip_ipv6_brackets(host: &str) -> &str {
    let host = host.trim();
    if host.starts_with('[') && host.ends_with(']') && host.len() >= 2 {
        &host[1..host.len() - 1]
    } else {
        host
    }
}

/// Classify a host string that needs no DNS resolution: `localhost`,
/// `*.localhost`, or a bare / bracketed IP literal. Returns `None` when the
/// host requires DNS.
pub fn classify_host_literal(host: &str) -> Option<NetClass> {
    let host = strip_ipv6_brackets(host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(classify_ip(&ip));
    }
    let lowered = host.to_lowercase();
    if lowered == "localhost" || lowered.ends_with(".localhost") {
        return Some(NetClass::Askable);
    }
    None
}

/// Fold the strictest grade across every resolved address.
fn strictest_of(addrs: &[IpAddr]) -> NetClass {
    addrs
        .iter()
        .map(classify_ip)
        .fold(NetClass::Public, NetClass::strictest)
}

/// Resolve `host` via blocking DNS and classify the strictest grade across
/// all resolved addresses. Returns `None` on resolution failure.
pub fn resolve_and_classify(host: &str) -> Option<(NetClass, Vec<IpAddr>)> {
    if let Some(cached) = dns_cache_get(host) {
        return Some(cached);
    }
    let addrs: Vec<IpAddr> = (host, 0)
        .to_socket_addrs()
        .ok()?
        .map(|socket| socket.ip())
        .collect();
    if addrs.is_empty() {
        return None;
    }
    let result = (strictest_of(&addrs), addrs);
    dns_cache_put(host, result.clone());
    Some(result)
}

/// Async variant of [`resolve_and_classify`] with a 5-second DNS timeout.
pub async fn resolve_and_classify_async(host: &str) -> Option<(NetClass, Vec<IpAddr>)> {
    if let Some(cached) = dns_cache_get(host) {
        return Some(cached);
    }
    let lookup = tokio::time::timeout(DNS_LOOKUP_TIMEOUT, tokio::net::lookup_host((host, 0))).await;
    let sockets = lookup.ok()?.ok()?;
    let addrs: Vec<IpAddr> = sockets.map(|socket| socket.ip()).collect();
    if addrs.is_empty() {
        return None;
    }
    let result = (strictest_of(&addrs), addrs);
    dns_cache_put(host, result.clone());
    Some(result)
}

/// Read a cached DNS classification result (30s TTL, 256-entry capacity).
pub fn dns_cache_get(host: &str) -> Option<(NetClass, Vec<IpAddr>)> {
    dns_cache().get(host)
}

/// Store a DNS classification result in the cache (30s TTL, 256-entry
/// capacity; the cache is wholesale rebuilt when full).
pub fn dns_cache_put(host: &str, value: (NetClass, Vec<IpAddr>)) {
    dns_cache().put(host, value);
}

/// Extract the lowercase, port-stripped, bracket-stripped host from a URL
/// string. Returns `None` when the string does not parse as a URL with a
/// host.
pub fn host_of_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(strip_ipv6_brackets(host).to_lowercase())
}

/// The permission-facet string for a fetch host: `FetchURL(host:<host>)`.
pub fn fetch_host_facet(host: &str) -> String {
    format!("FetchURL(host:{})", host.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_classify_metadata_when_ipv4_metadata_endpoint() {
        assert_eq!(
            classify_ip(&"169.254.169.254".parse().unwrap()),
            NetClass::Metadata
        );
    }

    #[test]
    fn should_classify_metadata_when_ipv4_mapped_ipv6() {
        assert_eq!(
            classify_ip(&"::ffff:169.254.169.254".parse().unwrap()),
            NetClass::Metadata
        );
    }

    #[test]
    fn should_classify_askable_when_cgnat_range() {
        assert_eq!(
            classify_ip(&"100.100.1.1".parse().unwrap()),
            NetClass::Askable
        );
    }

    #[test]
    fn should_classify_askable_when_link_local_non_metadata() {
        assert_eq!(
            classify_ip(&"169.254.0.1".parse().unwrap()),
            NetClass::Askable
        );
    }

    #[test]
    fn should_classify_public_when_public_ipv4() {
        for s in ["8.8.8.8", "1.1.1.1", "203.0.113.1"] {
            assert_eq!(classify_ip(&s.parse().unwrap()), NetClass::Public);
        }
    }

    #[test]
    fn should_classify_askable_when_ipv6_private_segments() {
        for s in ["::1", "fe80::1", "fc00::1", "fd00::1"] {
            assert_eq!(classify_ip(&s.parse().unwrap()), NetClass::Askable);
        }
    }

    #[test]
    fn should_classify_metadata_when_ipv6_ec2_endpoint() {
        assert_eq!(
            classify_ip(&"fd00:ec2::254".parse().unwrap()),
            NetClass::Metadata
        );
    }

    #[test]
    fn should_classify_host_literal_when_localhost() {
        assert_eq!(classify_host_literal("localhost"), Some(NetClass::Askable));
        assert_eq!(
            classify_host_literal("foo.localhost"),
            Some(NetClass::Askable)
        );
        assert_eq!(classify_host_literal("LOCALHOST"), Some(NetClass::Askable));
    }

    #[test]
    fn should_classify_host_literal_when_ip_literal() {
        assert_eq!(classify_host_literal("127.0.0.1"), Some(NetClass::Askable));
        assert_eq!(
            classify_host_literal("169.254.169.254"),
            Some(NetClass::Metadata)
        );
        assert_eq!(classify_host_literal("8.8.8.8"), Some(NetClass::Public));
    }

    #[test]
    fn should_classify_host_literal_when_bracketed_ipv6() {
        assert_eq!(classify_host_literal("[::1]"), Some(NetClass::Askable));
        assert_eq!(
            classify_host_literal("[::ffff:169.254.169.254]"),
            Some(NetClass::Metadata)
        );
    }

    #[test]
    fn should_return_none_when_host_needs_dns() {
        assert_eq!(classify_host_literal("example.com"), None);
    }

    #[test]
    fn should_take_strictest_when_classes_mixed() {
        assert_eq!(
            NetClass::Public.strictest(NetClass::Askable),
            NetClass::Askable
        );
        assert_eq!(
            NetClass::Askable.strictest(NetClass::Public),
            NetClass::Askable
        );
        assert_eq!(
            NetClass::Askable.strictest(NetClass::Metadata),
            NetClass::Metadata
        );
    }

    #[test]
    fn should_normalize_host_when_url_has_port_and_case() {
        assert_eq!(
            host_of_url("http://MyHost.Example:8080/a").as_deref(),
            Some("myhost.example")
        );
        assert_eq!(host_of_url("not a url"), None);
    }

    #[test]
    fn should_strip_brackets_when_url_has_ipv6_literal() {
        assert_eq!(host_of_url("http://[::1]:8080/x").as_deref(), Some("::1"));
        assert_eq!(
            host_of_url("http://[fe80::1]/x").as_deref(),
            Some("fe80::1")
        );
    }

    #[test]
    fn should_report_segment_name_when_classifying() {
        assert_eq!(segment_name(&"127.0.0.1".parse().unwrap()), "loopback");
        assert_eq!(segment_name(&"10.0.0.1".parse().unwrap()), "RFC1918");
        assert_eq!(segment_name(&"100.100.1.1".parse().unwrap()), "CGNAT");
        assert_eq!(segment_name(&"169.254.0.1".parse().unwrap()), "link-local");
        assert_eq!(segment_name(&"fc00::1".parse().unwrap()), "IPv6 ULA");
        assert_eq!(
            segment_name(&"169.254.169.254".parse().unwrap()),
            "cloud metadata"
        );
        assert_eq!(segment_name(&"8.8.8.8".parse().unwrap()), "public");
    }

    #[test]
    fn should_report_ipv6_segment_names_when_classifying() {
        assert_eq!(segment_name(&"::1".parse().unwrap()), "loopback");
        assert_eq!(segment_name(&"fe80::1".parse().unwrap()), "IPv6 link-local");
        assert_eq!(
            segment_name(&"fd00:ec2::254".parse().unwrap()),
            "cloud metadata"
        );
        assert_eq!(segment_name(&"::ffff:10.0.0.1".parse().unwrap()), "RFC1918");
    }

    #[test]
    fn should_report_unspecified_when_zero_network_ipv4() {
        assert_eq!(segment_name(&"0.0.0.5".parse().unwrap()), "unspecified");
        assert_eq!(classify_ip(&"0.1.2.3".parse().unwrap()), NetClass::Askable);
    }

    #[test]
    fn should_roundtrip_when_dns_cache_put_and_get() {
        let host = "cache-test.invalid";
        let value = (
            NetClass::Askable,
            vec!["10.0.0.1".parse().unwrap(), "8.8.8.8".parse().unwrap()],
        );
        dns_cache_put(host, value.clone());
        assert_eq!(dns_cache_get(host), Some(value));
        assert_eq!(dns_cache_get("other-test.invalid"), None);
    }

    #[test]
    fn should_take_strictest_when_resolved_addresses_mixed() {
        let addrs: Vec<IpAddr> = ["8.8.8.8", "10.0.0.1", "169.254.169.254"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(strictest_of(&addrs), NetClass::Metadata);
    }

    #[test]
    fn should_format_facet_when_host_given() {
        assert_eq!(fetch_host_facet("MyHost"), "FetchURL(host:myhost)");
    }
}

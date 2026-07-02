use std::net::IpAddr;
use std::sync::OnceLock;

// Cloudflare's published edge ranges (https://www.cloudflare.com/ips); the reference board sits behind
// Cloudflare, so a client-IP header is believed only from these peers. BAKEMONO_TRUSTED_PROXIES overrides
// with a comma-separated CIDR list; set it empty to trust no proxy and key on the socket peer directly
const CLOUDFLARE_CIDRS: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

// true when the socket peer may set the forwarding header; a direct client that is not a trusted proxy
// cannot forge its own client IP, so its rate-limit key stays its real address
pub fn is_trusted_proxy(peer: IpAddr) -> bool {
    let peer = peer.to_canonical();
    ranges().iter().any(|r| r.contains(peer))
}

// unset or empty falls back to Cloudflare (harmless off-Cloudflare, since no real peer lands in those
// ranges); the literal `none` trusts no proxy; anything else is the operator's own CIDR list
fn ranges() -> &'static [Cidr] {
    static RANGES: OnceLock<Vec<Cidr>> = OnceLock::new();
    RANGES.get_or_init(|| {
        let cloudflare = || CLOUDFLARE_CIDRS.iter().filter_map(|s| Cidr::parse(s)).collect();
        match std::env::var("BAKEMONO_TRUSTED_PROXIES") {
            Ok(list) if list.trim().eq_ignore_ascii_case("none") => Vec::new(),
            Ok(list) if !list.trim().is_empty() => {
                list.split(',').filter_map(|s| Cidr::parse(s.trim())).collect()
            }
            _ => cloudflare(),
        }
    })
}

enum Cidr {
    V4 { net: u32, mask: u32 },
    V6 { net: u128, mask: u128 },
}

impl Cidr {
    fn parse(s: &str) -> Option<Self> {
        let (addr, prefix) = s.split_once('/')?;
        let prefix: u32 = prefix.parse().ok()?;
        match addr.parse::<IpAddr>().ok()? {
            IpAddr::V4(a) if prefix <= 32 => {
                let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
                Some(Cidr::V4 { net: u32::from(a) & mask, mask })
            }
            IpAddr::V6(a) if prefix <= 128 => {
                let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
                Some(Cidr::V6 { net: u128::from(a) & mask, mask })
            }
            _ => None,
        }
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Cidr::V4 { net, mask }, IpAddr::V4(a)) => u32::from(a) & mask == *net,
            (Cidr::V6 { net, mask }, IpAddr::V6(a)) => u128::from(a) & mask == *net,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_range_is_trusted_direct_client_is_not() {
        // an address inside 104.16.0.0/13 is a Cloudflare edge; a random public host is not
        assert!(is_trusted_proxy("104.16.0.1".parse().unwrap()));
        assert!(is_trusted_proxy("162.158.1.1".parse().unwrap()));
        assert!(!is_trusted_proxy("8.8.8.8".parse().unwrap()));
        assert!(!is_trusted_proxy("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn ipv4_mapped_peer_matches_v4_range() {
        assert!(is_trusted_proxy("::ffff:104.16.0.1".parse().unwrap()));
    }

    #[test]
    fn parses_v4_and_v6_cidrs() {
        assert!(Cidr::parse("104.16.0.0/13").is_some());
        assert!(Cidr::parse("2400:cb00::/32").is_some());
        assert!(Cidr::parse("104.16.0.0/33").is_none());
        assert!(Cidr::parse("nonsense").is_none());
    }
}

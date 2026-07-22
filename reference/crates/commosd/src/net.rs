//! Network trust classification (Volume 9 — secure defaults with zero operator burden).
//!
//! CommOS ships to run on a box on a private LAN (a Raspberry Pi, an on-prem server). The
//! zero-config conveniences that make that pleasant — the `tenant:<uuid>` dev bearer, admin
//! dev-mode, the unauthenticated introspection feed, phone auto-provisioning, and open SIP
//! registration — are safe *on that trusted network* but catastrophic if the same box is
//! reachable from the public internet. Rather than force the operator to flip a wall of
//! hardening flags (which they will forget), the daemon classifies each request/packet by its
//! **peer address**: loopback and private/link-local ranges are treated as the trusted LAN and
//! keep working exactly as before, while traffic from a public address must authenticate.
//!
//! This mirrors the posture the docs already assume ("keep SIP off the public internet",
//! "enable auth before exposing beyond a trusted network") — it just enforces it
//! automatically instead of on the honour system.
//!
//! Note on reverse proxies: this looks at the immediate transport peer only. A forwarded
//! header (`X-Forwarded-For`) is attacker-settable and is deliberately never consulted here —
//! trust must not be widenable by a header. Terminate auth/TLS at the proxy when exposing
//! CommOS publicly behind one on the same host.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Is this IP on a trusted network (loopback, private, or link-local)?
///
/// Trusted ⇒ the request originated on the local host or the private LAN CommOS is deployed
/// on. Anything else (a routable/public address) is untrusted and must authenticate.
pub fn is_trusted_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_trusted_v4(v4),
        IpAddr::V6(v6) => {
            // Loopback first: `::1` must not be misread as an IPv4-compatible address.
            if v6.is_loopback() {
                return true;
            }
            // Unwrap an IPv4-mapped address (`::ffff:192.168.0.5`) so it is classified by its
            // embedded v4 address rather than treated as a bare (public) v6.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_trusted_v4(&v4);
            }
            is_unique_local_v6(v6) || is_link_local_v6(v6)
        }
    }
}

fn is_trusted_v4(v4: &Ipv4Addr) -> bool {
    v4.is_loopback() || v4.is_private() || v4.is_link_local()
}

/// Should an *outbound* connection to `ip` be refused? Used to stop server-side request forgery
/// (SSRF) via operator-supplied targets (e.g. webhook URLs): a target that resolves to the local
/// host, the private LAN, link-local space (incl. the `169.254.169.254` cloud metadata service),
/// or an unspecified/multicast/broadcast address is blocked. Only routable public unicast
/// destinations are allowed. Callers should resolve the hostname and vet the concrete IP they are
/// about to connect to (pinning it), so DNS rebinding cannot swap a vetted IP for a blocked one.
pub fn is_disallowed_egress(ip: &IpAddr) -> bool {
    if is_trusted_ip(ip) {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => {
            v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() || v4.is_documentation()
        }
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast(),
    }
}

/// Unique-local address (`fc00::/7`) — the IPv6 analogue of RFC1918 private space.
fn is_unique_local_v6(v6: &Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// Link-local unicast (`fe80::/10`).
fn is_link_local_v6(v6: &Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_private_v4_are_trusted() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.16.9.9", "192.168.1.50", "169.254.0.7"] {
            assert!(is_trusted_ip(&ip.parse().unwrap()), "{ip} should be trusted");
        }
    }

    #[test]
    fn public_v4_is_untrusted() {
        for ip in ["8.8.8.8", "1.1.1.1", "203.0.113.5", "172.32.0.1", "192.169.0.1"] {
            assert!(!is_trusted_ip(&ip.parse().unwrap()), "{ip} should be untrusted");
        }
    }

    #[test]
    fn v6_loopback_ula_linklocal_trusted_public_not() {
        assert!(is_trusted_ip(&"::1".parse().unwrap()));
        assert!(is_trusted_ip(&"fd00::1".parse().unwrap())); // unique-local
        assert!(is_trusted_ip(&"fe80::1".parse().unwrap())); // link-local
        assert!(!is_trusted_ip(&"2001:4860:4860::8888".parse().unwrap())); // public
    }

    #[test]
    fn v4_mapped_v6_is_classified_by_embedded_v4() {
        assert!(is_trusted_ip(&"::ffff:192.168.0.5".parse().unwrap()));
        assert!(!is_trusted_ip(&"::ffff:8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn egress_blocks_internal_and_metadata() {
        for ip in [
            "127.0.0.1",        // loopback
            "10.0.0.5",         // private
            "192.168.1.1",      // private
            "169.254.169.254",  // cloud metadata (link-local)
            "0.0.0.0",          // unspecified
            "224.0.0.1",        // multicast
            "::1",              // v6 loopback
            "fd00::1",          // v6 ULA
        ] {
            assert!(is_disallowed_egress(&ip.parse().unwrap()), "{ip} should be blocked");
        }
    }

    #[test]
    fn egress_allows_public() {
        // Genuinely routable public addresses (note: 203.0.113.0/24 is TEST-NET documentation
        // space and is intentionally blocked, so it is not used here).
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2001:4860:4860::8888"] {
            assert!(!is_disallowed_egress(&ip.parse().unwrap()), "{ip} should be allowed");
        }
    }
}

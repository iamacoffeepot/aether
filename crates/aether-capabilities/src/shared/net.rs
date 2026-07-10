//! Shared networking helpers used by more than one server capability.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Reconstructs a teardown self-connect target from state a server
/// capability already holds — no new state field. The listener's bound
/// IP equals the configured `bind_addr`'s IP (`bind` only resolves the
/// *port* when it is `0`), so this pairs that IP with the resolved
/// `listener_port`. A wildcard bind (`0.0.0.0` / `::`) has no address a
/// self-connect can land on, so it maps to the matching loopback
/// instead; an unparseable `bind_addr` falls back to IPv4 loopback.
#[must_use]
pub fn teardown_connect_addr(bind_addr: &str, listener_port: u16) -> SocketAddr {
    let ip = bind_addr.parse::<SocketAddr>().map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |addr| addr.ip());
    let ip = match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    SocketAddr::new(ip, listener_port)
}

#[cfg(test)]
mod unit_tests {
    use super::teardown_connect_addr;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    /// Tripwire: a wildcard bind (`0.0.0.0` / `::`, no routable self-connect
    /// target) maps to the matching loopback family — this is the actual bug
    /// fix (issue #2631), not a mirror of the input.
    #[test]
    fn teardown_connect_addr_maps_wildcard_to_loopback() {
        assert_eq!(teardown_connect_addr("0.0.0.0:8080", 8080), SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080));
        assert_eq!(teardown_connect_addr("[::]:8080", 8080), SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8080));
    }

    /// A specific bind IP (not wildcard) is preserved as-is, paired with the
    /// resolved listener port rather than whatever port `bind_addr` named
    /// (e.g. `0` for an OS-assigned port).
    #[test]
    fn teardown_connect_addr_preserves_specific_ip() {
        assert_eq!(
            teardown_connect_addr("192.168.1.5:0", 41_234),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 41_234)
        );
        assert_eq!(
            teardown_connect_addr("127.0.0.1:8080", 8080),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
        );
    }

    /// An unparseable `bind_addr` (e.g. a hostname `TcpListener::bind` would
    /// resolve via DNS, which this pure helper does not do) falls back to
    /// IPv4 loopback rather than panicking or propagating the parse error.
    #[test]
    fn teardown_connect_addr_unparseable_falls_back_to_loopback() {
        assert_eq!(
            teardown_connect_addr("not-an-address", 9090),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9090)
        );
        assert_eq!(
            teardown_connect_addr("localhost:9090", 9090),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9090)
        );
    }
}

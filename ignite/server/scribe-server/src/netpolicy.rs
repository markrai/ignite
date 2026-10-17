//! D1 product network boundary: ONE policy for what Scribe Server advertises (DNS-SD), what it
//! accepts connections on, and which interfaces the installer scopes firewall rules to.
//!
//! An interface address is **eligible** (the physical/private LAN) when
//! * its name is not a loopback / container / bridge / VM / VPN interface
//!   ([`NON_LAN_PREFIXES`]: `tailscale*`, `wg*`, `docker*`, `br-*`, `veth*`, …), and
//! * its address is private: IPv4 RFC 1918 (10/8, 172.16/12, 192.168/16) or IPv6 ULA
//!   (fc00::/7). Public, link-local (169.254/16, fe80::/10), CGNAT (100.64/10, e.g. Tailscale),
//!   loopback and unspecified addresses are not.
//!
//! A product-mode connection is accepted when
//! * the local (destination) address is an eligible interface address **and** the peer is on
//!   that interface's subnet (the same on-link scope DNS-SD discovery reaches; this also refuses
//!   traffic routed in over Tailscale/VPN/containers to the LAN address), or
//! * both ends are loopback (documented: local diagnostics on the server itself, e.g.
//!   `curl http://127.0.0.1/control/v1/status`). Loopback is never advertised.
//!
//! No address is persisted: the inventory is re-read from the OS (short cache; immediate
//! refresh on an unknown local address), so DHCP changes keep working. The developer
//! `scribe-sync-server` has no accept filter and is unchanged.

use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Interface-name prefixes that are never the LAN (loopback, containers, bridges, VMs, VPNs).
pub const NON_LAN_PREFIXES: [&str; 24] = [
    "lo",
    "docker",
    "br-",
    "veth",
    "virbr",
    "vnet",
    "tailscale",
    "tun",
    "tap",
    "wg",
    "zt",
    "cni",
    "flannel",
    "cali",
    "kube",
    "lxc",
    "lxd",
    "podman",
    "vmnet",
    "vboxnet",
    "utun",
    "llw",
    "awdl",
    "ham",
];

/// One interface address from the OS inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IfaceAddr {
    pub name: String,
    pub ip: IpAddr,
    pub prefix_len: u8,
}

pub fn is_lan_interface_name(name: &str) -> bool {
    !NON_LAN_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Private (LAN) unicast address per the D1 policy.
pub fn is_private_lan_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() && !v.is_link_local() && !v.is_loopback(),
        IpAddr::V6(v) => (v.segments()[0] & 0xfe00) == 0xfc00, // ULA fc00::/7
    }
}

pub fn is_eligible(a: &IfaceAddr) -> bool {
    is_lan_interface_name(&a.name) && is_private_lan_address(a.ip)
}

/// The eligible subset of an inventory — the single source for advertisement, the accept
/// policy and firewall scoping.
pub fn eligible(inventory: &[IfaceAddr]) -> Vec<IfaceAddr> {
    inventory
        .iter()
        .filter(|a| is_eligible(a))
        .cloned()
        .collect()
}

/// Eligible interface names (deduplicated, sorted) for firewall scoping.
pub fn eligible_interface_names(inventory: &[IfaceAddr]) -> Vec<String> {
    let mut n: Vec<String> = eligible(inventory).into_iter().map(|a| a.name).collect();
    n.sort();
    n.dedup();
    n
}

fn same_subnet(a: IpAddr, b: IpAddr, prefix: u8) -> bool {
    match (a, b) {
        (IpAddr::V4(x), IpAddr::V4(y)) => {
            let p = u32::from(prefix.min(32));
            let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
            (u32::from(x) & mask) == (u32::from(y) & mask)
        }
        (IpAddr::V6(x), IpAddr::V6(y)) => {
            let p = u32::from(prefix.min(128));
            let mask = if p == 0 { 0 } else { u128::MAX << (128 - p) };
            (u128::from(x) & mask) == (u128::from(y) & mask)
        }
        _ => false,
    }
}

/// Unwrap IPv4-mapped IPv6 (`::ffff:a.b.c.d`) so dual-stack sockets are judged by IPv4 rules.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Local diagnostics on the server itself.
    AllowLoopback,
    /// A client on the physical LAN.
    AllowLan,
    /// Arrived via a non-LAN interface/address (VPN, container, public, …).
    Refuse,
    /// The local address is not in the (possibly stale) inventory: refresh and re-decide.
    Unknown,
}

/// Pure accept decision for one connection.
pub fn decide(inventory: &[IfaceAddr], local: IpAddr, peer: IpAddr) -> Decision {
    let (local, peer) = (canonical(local), canonical(peer));
    if local.is_loopback() && peer.is_loopback() {
        return Decision::AllowLoopback;
    }
    let Some(owner) = inventory.iter().find(|a| a.ip == local) else {
        return Decision::Unknown;
    };
    if is_eligible(owner) && same_subnet(owner.ip, peer, owner.prefix_len) {
        Decision::AllowLan
    } else {
        Decision::Refuse
    }
}

/// The OS interface inventory (portable via `if-addrs`).
pub fn os_inventory() -> Vec<IfaceAddr> {
    if_addrs::get_if_addrs()
        .map(|v| {
            v.into_iter()
                .map(|i| {
                    let (ip, prefix_len) = match &i.addr {
                        if_addrs::IfAddr::V4(a) => (IpAddr::V4(a.ip), a.prefixlen),
                        if_addrs::IfAddr::V6(a) => (IpAddr::V6(a.ip), a.prefixlen),
                    };
                    IfaceAddr {
                        name: i.name,
                        ip,
                        prefix_len,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Runtime policy with a short-lived inventory cache (refreshed on an unknown local address).
pub struct LanPolicy {
    source: Box<dyn Fn() -> Vec<IfaceAddr> + Send + Sync>,
    cache: Mutex<(Option<Instant>, Vec<IfaceAddr>)>,
}

const INVENTORY_TTL: Duration = Duration::from_secs(10);

impl LanPolicy {
    pub fn new(source: impl Fn() -> Vec<IfaceAddr> + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
            cache: Mutex::new((None, Vec::new())),
        }
    }

    pub fn for_this_host() -> Self {
        Self::new(os_inventory)
    }

    fn inventory(&self, force: bool) -> Vec<IfaceAddr> {
        let mut g = self.cache.lock().unwrap();
        if force || g.0.is_none_or(|t| t.elapsed() >= INVENTORY_TTL) {
            *g = (Some(Instant::now()), (self.source)());
        }
        g.1.clone()
    }

    pub fn eligible(&self) -> Vec<IfaceAddr> {
        eligible(&self.inventory(true))
    }

    /// Accept-path check for one connection.
    pub fn allows(&self, local: SocketAddr, peer: SocketAddr) -> bool {
        match decide(&self.inventory(false), local.ip(), peer.ip()) {
            Decision::AllowLan | Decision::AllowLoopback => true,
            Decision::Refuse => false,
            Decision::Unknown => matches!(
                decide(&self.inventory(true), local.ip(), peer.ip()),
                Decision::AllowLan | Decision::AllowLoopback
            ),
        }
    }
}

/// Synthetic multi-interface inventory for tests and documentation.
/// Preserves the original category mix (loopback, one eligible RFC1918 LAN,
/// CGNAT/VPN, container/bridge, veth link-local, WireGuard, IPv6 link-local)
/// without embedding any real host inventory.
#[doc(hidden)]
pub fn synthetic_multi_interface_inventory() -> Vec<IfaceAddr> {
    let a = |name: &str, ip: &str, p: u8| IfaceAddr {
        name: name.into(),
        ip: ip.parse().unwrap(),
        prefix_len: p,
    };
    vec![
        a("lo", "127.0.0.1", 8),
        a("eth0", "192.168.57.10", 24),
        a("tailscale0", "100.96.10.5", 32),
        a("docker0", "172.19.0.1", 16),
        a("br-synth01", "172.18.0.1", 16),
        a("vethaabb01", "169.254.10.4", 16),
        a("wg0", "10.77.0.1", 24),
        a("eth0", "fe80::1", 64),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn a(name: &str, addr: &str, p: u8) -> IfaceAddr {
        IfaceAddr {
            name: name.into(),
            ip: ip(addr),
            prefix_len: p,
        }
    }

    #[test]
    fn classifier() {
        assert!(is_eligible(&a("eth0", "192.168.57.10", 24)));
        assert!(is_eligible(&a("wlp2s0", "10.77.1.7", 8)));
        assert!(is_eligible(&a("en0", "172.20.10.2", 28)));
        assert!(is_eligible(&a("eth0", "fd12:3456::9", 64)));
        assert!(
            !is_eligible(&a("lo", "127.0.0.1", 8)),
            "loopback is never advertised"
        );
        assert!(!is_eligible(&a("tailscale0", "100.96.10.5", 32)));
        assert!(
            !is_eligible(&a("tailscale0", "192.168.50.2", 24)),
            "name alone excludes VPNs"
        );
        assert!(!is_eligible(&a("wg0", "10.77.0.1", 24)));
        assert!(!is_eligible(&a("docker0", "172.19.0.1", 16)));
        assert!(!is_eligible(&a("br-synth01", "172.18.0.1", 16)));
        assert!(!is_eligible(&a("veth12ab", "10.1.2.3", 24)));
        assert!(
            !is_eligible(&a("eth0", "169.254.7.7", 16)),
            "IPv4 link-local"
        );
        assert!(!is_eligible(&a("eth0", "fe80::1", 64)), "IPv6 link-local");
        assert!(
            !is_eligible(&a("eth0", "203.0.113.5", 24)),
            "public address"
        );
        assert!(
            !is_eligible(&a("eth0", "100.64.1.1", 10)),
            "CGNAT is not the LAN"
        );
    }

    #[test]
    fn accept_policy_on_synthetic_inventory() {
        let inv = synthetic_multi_interface_inventory();
        // LAN client → LAN address: allowed.
        assert_eq!(
            decide(&inv, ip("192.168.57.10"), ip("192.168.57.20")),
            Decision::AllowLan
        );
        assert_eq!(
            decide(&inv, ip("::ffff:192.168.57.10"), ip("::ffff:192.168.57.30")),
            Decision::AllowLan
        );
        // Tailscale peer → Tailscale address: refused.
        assert_eq!(
            decide(&inv, ip("100.96.10.5"), ip("100.96.20.2")),
            Decision::Refuse
        );
        // Tailscale subnet-routed / container traffic to the LAN address: refused (off-subnet peer).
        assert_eq!(
            decide(&inv, ip("192.168.57.10"), ip("100.96.20.2")),
            Decision::Refuse
        );
        assert_eq!(
            decide(&inv, ip("192.168.57.10"), ip("172.19.0.5")),
            Decision::Refuse
        );
        // Container bridge / wg / link-local destinations: refused.
        assert_eq!(
            decide(&inv, ip("172.19.0.1"), ip("172.19.0.5")),
            Decision::Refuse
        );
        assert_eq!(
            decide(&inv, ip("172.18.0.1"), ip("172.18.0.9")),
            Decision::Refuse
        );
        assert_eq!(
            decide(&inv, ip("10.77.0.1"), ip("10.77.0.2")),
            Decision::Refuse
        );
        assert_eq!(decide(&inv, ip("fe80::1"), ip("fe80::2")), Decision::Refuse);
        // Documented: loopback diagnostics on the server itself are allowed.
        assert_eq!(
            decide(&inv, ip("127.0.0.1"), ip("127.0.0.1")),
            Decision::AllowLoopback
        );
        assert_eq!(decide(&inv, ip("::1"), ip("::1")), Decision::AllowLoopback);
        // An address not (yet) in the inventory must be re-checked, never assumed.
        assert_eq!(
            decide(&inv, ip("192.168.57.77"), ip("192.168.57.20")),
            Decision::Unknown
        );
    }

    #[test]
    fn dhcp_change_is_followed_without_persisted_addresses() {
        let state = std::sync::Arc::new(Mutex::new(vec![a("eth0", "192.168.57.10", 24)]));
        let s = state.clone();
        let p = LanPolicy::new(move || s.lock().unwrap().clone());
        let sock = |i: &str| SocketAddr::new(ip(i), 80);
        assert!(p.allows(sock("192.168.57.10"), sock("192.168.57.20")));
        *state.lock().unwrap() = vec![
            a("eth0", "192.168.57.20", 24),
            a("tailscale0", "100.96.10.5", 32),
        ];
        // Unknown local address → immediate refresh → the new LAN address is served.
        assert!(p.allows(sock("192.168.57.20"), sock("192.168.57.30")));
        assert!(!p.allows(sock("100.96.10.5"), sock("100.64.0.2")));
    }

    #[test]
    fn one_policy_feeds_advertisement_accept_and_firewall() {
        let inv = synthetic_multi_interface_inventory();
        let el = eligible(&inv);
        assert_eq!(el, vec![a("eth0", "192.168.57.10", 24)]);
        assert_eq!(eligible_interface_names(&inv), vec!["eth0".to_string()]);
        // Every advertised address is acceptable for its subnet; nothing else is.
        for e in &el {
            assert_eq!(decide(&inv, e.ip, e.ip), Decision::AllowLan);
        }
        for x in inv
            .iter()
            .filter(|x| !el.contains(x) && !x.ip.is_loopback())
        {
            assert_eq!(decide(&inv, x.ip, x.ip), Decision::Refuse, "{x:?}");
        }
        assert!(is_private_lan_address(IpAddr::V4(Ipv4Addr::new(
            192, 168, 0, 1
        ))));
        assert!(!is_private_lan_address(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }
}

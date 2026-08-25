//! Interface enumeration and pure CIDR math for /24 target generation.
//!
//! The shipped scope clamps every interface address into its containing /24
//! regardless of reported prefix (spec REQ-1 clamp scenario); `Subnet.prefix`
//! exists so a future manual-CIDR field needs no type change.

use if_addrs::{IfAddr, get_if_addrs};
use std::collections::HashSet;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceInfo {
    pub name: String,
    pub ip: Ipv4Addr,
}

/// IPv4 subnet with an invariant floor of /24; `network` is host-bit-masked
/// by [`subnet_of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub network: u32,
    pub prefix: u8,
}

/// IPv4 interfaces from if-addrs: non-loopback, deduplicated by address.
pub fn list_interfaces() -> Vec<InterfaceInfo> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for iface in get_if_addrs().unwrap_or_default() {
        let IfAddr::V4(v4) = iface.addr else {
            continue;
        };
        let ip = v4.ip;
        if ip.is_loopback() || !seen.insert(ip) {
            continue;
        }
        result.push(InterfaceInfo {
            name: iface.name,
            ip,
        });
    }
    result
}

/// Rejects prefixes below the /24 floor (defensive guard for a future
/// manual-CIDR field) and masks off host bits.
pub fn subnet_of(ip: Ipv4Addr, prefix: u8) -> Option<Subnet> {
    if !(24..=32).contains(&prefix) {
        return None;
    }
    Some(Subnet {
        network: u32::from(ip) & mask_of(prefix),
        prefix,
    })
}

/// Host addresses of `subnet`, excluding network and broadcast. A /24
/// yields `.1` through `.254` ascending; uniform exclusion makes /32 empty.
pub fn host_addresses(subnet: Subnet) -> Vec<Ipv4Addr> {
    let mask = mask_of(subnet.prefix);
    let base = subnet.network & mask;
    let broadcast = base | !mask;
    ((u64::from(base) + 1)..u64::from(broadcast))
        .map(|value| Ipv4Addr::from(value as u32))
        .collect()
}

/// Index of the default interface: first RFC1918 address, else first entry.
pub fn pick_default_subnet(ifaces: &[InterfaceInfo]) -> Option<usize> {
    if let Some(pos) = ifaces.iter().position(|iface| iface.ip.is_private()) {
        return Some(pos);
    }
    if ifaces.is_empty() { None } else { Some(0) }
}

fn mask_of(prefix: u8) -> u32 {
    if prefix == 0 {
        return 0;
    }
    u32::MAX << (32 - prefix.min(32))
}

#[cfg(test)]
mod tests {
    use super::{InterfaceInfo, Subnet, host_addresses, pick_default_subnet, subnet_of};
    use std::net::Ipv4Addr;

    fn iface(ip: [u8; 4]) -> InterfaceInfo {
        InterfaceInfo {
            name: "eth0".to_owned(),
            ip: Ipv4Addr::from(ip),
        }
    }

    #[test]
    fn slash24_yields_254_hosts_excluding_network_and_broadcast() {
        let subnet = Subnet {
            network: u32::from(Ipv4Addr::new(192, 168, 1, 0)),
            prefix: 24,
        };
        let hosts = host_addresses(subnet);
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(192, 168, 1, 254));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 0)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 255)));
    }

    #[test]
    fn host_generation_is_deterministic_and_ascending() {
        let subnet = Subnet {
            network: u32::from(Ipv4Addr::new(10, 0, 0, 0)),
            prefix: 24,
        };
        assert_eq!(host_addresses(subnet), host_addresses(subnet));
        let hosts = host_addresses(subnet);
        for pair in hosts.windows(2) {
            assert!(pair[0] < pair[1], "addresses must ascend");
        }
    }

    #[test]
    fn slash30_edge_count_is_two_hosts() {
        let subnet = Subnet {
            network: u32::from(Ipv4Addr::new(192, 168, 1, 0)),
            prefix: 30,
        };
        assert_eq!(
            host_addresses(subnet),
            vec![Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 2)]
        );
    }

    #[test]
    fn slash32_edge_count_is_empty_host_range() {
        // Uniform exclusion rule: network == broadcast == the single address,
        // so no scannable host range remains.
        let subnet = Subnet {
            network: u32::from(Ipv4Addr::new(10, 0, 0, 5)),
            prefix: 32,
        };
        assert!(host_addresses(subnet).is_empty());
    }

    #[test]
    fn subnet_of_rejects_prefix_below_floor() {
        // Defensive constructor guard for the future manual-CIDR field;
        // unreachable from the wizard path (Reconciliation 5).
        assert_eq!(subnet_of(Ipv4Addr::new(10, 9, 8, 7), 23), None);
        assert_eq!(subnet_of(Ipv4Addr::new(10, 9, 8, 7), 16), None);
    }

    #[test]
    fn subnet_of_accepts_prefix_at_floor_and_ceiling() {
        assert!(subnet_of(Ipv4Addr::new(10, 9, 8, 7), 24).is_some());
        assert!(subnet_of(Ipv4Addr::new(10, 9, 8, 7), 32).is_some());
    }

    #[test]
    fn clamp_derives_containing_slash24_from_wider_interface_prefix() {
        // REQ-1 clamp scenario: an interface addressed inside a wider prefix
        // always derives its containing /24 as the candidate subnet.
        let derived = subnet_of(Ipv4Addr::new(10, 9, 8, 7), 24).expect("containing /24");
        assert_eq!(
            derived.network,
            u32::from(Ipv4Addr::new(10, 9, 8, 0)),
            "host bits must be masked off"
        );
        let hosts = host_addresses(derived);
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(10, 9, 8, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(10, 9, 8, 254));
    }

    #[test]
    fn pick_default_subnet_empty_yields_none() {
        assert_eq!(pick_default_subnet(&[]), None);
    }

    #[test]
    fn pick_default_prefers_first_rfc1918_over_public() {
        let ifaces = vec![iface([203, 0, 113, 5]), iface([10, 0, 0, 2])];
        assert_eq!(pick_default_subnet(&ifaces), Some(1));
    }

    #[test]
    fn pick_default_takes_first_when_all_rfc1918() {
        let ifaces = vec![iface([172, 16, 0, 1]), iface([192, 168, 1, 20])];
        assert_eq!(pick_default_subnet(&ifaces), Some(0));
    }

    #[test]
    fn pick_default_falls_back_to_first_public_only() {
        let ifaces = vec![iface([203, 0, 113, 5]), iface([198, 51, 100, 7])];
        assert_eq!(pick_default_subnet(&ifaces), Some(0));
    }
}

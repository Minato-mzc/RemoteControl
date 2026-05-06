//! Best-effort LAN IPv4 discovery for the QR payload.
//!
//! `local_ip_address::local_ip()` picks whichever interface owns the default
//! route, which is wrong in two common cases on this user's machine:
//!   1. A `vEthernet (Default Switch)` virtual NIC at 172.31.80.1 may win the
//!      tie-break over the real Wi-Fi adapter, advertising an IP the phone
//!      cannot reach.
//!   2. When the PC tethers off the phone's hotspot, the Wi-Fi adapter gets a
//!      192.168.43.x address and the previous primary IP becomes stale.
//!
//! We instead enumerate every interface, drop non-routable / loopback /
//! link-local / known-virtual ones, and rank the survivors so the QR HTML can
//! show one tile per candidate. The user picks the one they're actually on.

use anyhow::Result;
use std::net::Ipv4Addr;

#[derive(Debug, Clone)]
pub struct DiscoveredAddr {
    pub addr: Ipv4Addr,
    pub iface_name: String,
    pub kind: InterfaceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    /// Physical or Wi-Fi adapter (preferred).
    Physical,
    /// Hyper-V / VMware / WSL / VirtualBox bridge — usually unreachable from a phone.
    Virtual,
    /// Couldn't classify by name; treated as a fallback ahead of Virtual.
    Unknown,
}

pub fn discover_lan_ipv4() -> Result<Vec<DiscoveredAddr>> {
    let mut out = Vec::new();
    for ifa in if_addrs::get_if_addrs()? {
        if ifa.is_loopback() {
            continue;
        }
        let v4 = match ifa.ip() {
            std::net::IpAddr::V4(v) => v,
            _ => continue,
        };
        if v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() {
            continue;
        }
        if !is_private(v4) {
            // Public IPs on the host are extremely unusual for a LAN-only product;
            // skip them to avoid accidentally advertising one.
            continue;
        }
        let kind = classify_interface(&ifa.name);
        out.push(DiscoveredAddr {
            addr: v4,
            iface_name: ifa.name,
            kind,
        });
    }
    out.sort_by_key(|a| match a.kind {
        InterfaceKind::Physical => 0,
        InterfaceKind::Unknown => 1,
        InterfaceKind::Virtual => 2,
    });
    Ok(out)
}

fn classify_interface(name: &str) -> InterfaceKind {
    let n = name.to_ascii_lowercase();
    let virtual_markers = [
        "vethernet",
        "virtualbox",
        "vmware",
        "hyper-v",
        "wsl",
        "loopback pseudo",
        "docker",
    ];
    if virtual_markers.iter().any(|m| n.contains(m)) {
        return InterfaceKind::Virtual;
    }
    let physical_markers = [
        "wi-fi", "wlan", "wireless", "ethernet", "以太网", "无线",
    ];
    if physical_markers.iter().any(|m| n.contains(m)) {
        return InterfaceKind::Physical;
    }
    InterfaceKind::Unknown
}

fn is_private(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    matches!(o, [10, ..])
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_names() {
        assert_eq!(classify_interface("Wi-Fi"), InterfaceKind::Physical);
        assert_eq!(classify_interface("WLAN"), InterfaceKind::Physical);
        assert_eq!(classify_interface("以太网"), InterfaceKind::Physical);
        assert_eq!(
            classify_interface("vEthernet (Default Switch)"),
            InterfaceKind::Virtual
        );
        assert_eq!(
            classify_interface("VMware Network Adapter VMnet1"),
            InterfaceKind::Virtual
        );
        assert_eq!(classify_interface("eth0"), InterfaceKind::Unknown);
    }

    #[test]
    fn private_recognition() {
        assert!(is_private(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(is_private(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_private(Ipv4Addr::new(172, 16, 0, 1)));
        assert!(is_private(Ipv4Addr::new(172, 31, 80, 1)));
        assert!(!is_private(Ipv4Addr::new(172, 32, 0, 1)));
        assert!(!is_private(Ipv4Addr::new(8, 8, 8, 8)));
    }
}

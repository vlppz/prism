//! What Android's VpnService.Builder gets for a config: addresses, DNS and
//! routes, including smart routing's exclusions.

use std::net::{IpAddr, Ipv4Addr};

use prism_engine::config::Cidr;
use prism_engine::WgConfig;
use serde::Serialize;

/// What VpnService.Builder gets.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub session: String,
    pub mtu: u16,
    pub addresses: Vec<String>,
    pub dns: Vec<String>,
    /// Routes into the tunnel.
    pub routes: Vec<String>,
    /// Routes that stay outside it (smart routing; Android 13+ only).
    pub excludes: Vec<String>,
}

/// Android sends the whole route table to every app that watches the network,
/// so it can't take the ~13k Russian networks. The largest blocks carry most
/// of the address space; the rest simply goes through the tunnel.
const MAX_EXCLUDES: usize = 1500;

pub fn plan(cfg: &WgConfig, smart: bool) -> Plan {
    let has_v6 = cfg.addresses.iter().any(|a| a.addr.is_ipv6());
    let mut routes: Vec<Cidr> = cfg.peer.allowed_ips.clone();
    // Same rule as the desktop engine: an IPv4-only full tunnel captures IPv6
    // too, and the engine refuses it instead of letting it leak.
    let full_v4 = routes.iter().any(|c| c.is_default() && c.addr.is_ipv4());
    if full_v4 && !has_v6 && !routes.iter().any(|c| c.is_default() && c.addr.is_ipv6()) {
        routes.push(Cidr { addr: IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), prefix: 0 });
    }
    let excludes = if smart && full_v4 { largest(prism_engine::geoip::russia_v4(), MAX_EXCLUDES) } else { Vec::new() };
    Plan {
        session: format!("Prism · {}", cfg.endpoint_host()),
        mtu: cfg.mtu.unwrap_or(1280),
        addresses: cfg.addresses.iter().map(|c| c.to_string()).collect(),
        dns: cfg.dns.iter().filter(|d| d.is_ipv4() || has_v6).map(|d| d.to_string()).collect(),
        routes: routes.iter().map(|c| c.to_string()).collect(),
        excludes: excludes.iter().map(|(a, p)| format!("{a}/{p}")).collect(),
    }
}

fn largest(mut nets: Vec<(Ipv4Addr, u8)>, n: usize) -> Vec<(Ipv4Addr, u8)> {
    nets.sort_by_key(|&(a, p)| (p, u32::from(a)));
    nets.truncate(n);
    nets.sort_by_key(|&(a, _)| u32::from(a));
    nets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_full_tunnel() {
        let cfg = prism_engine::parse(
            "[Interface]\nPrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\nAddress = 10.0.0.2/32\nDNS = 1.1.1.1, 2606:4700::1111\n\
             [Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nAllowedIPs = 0.0.0.0/0\nEndpoint = 203.0.113.1:51820\n",
        )
        .unwrap();
        let p = plan(&cfg, true);
        assert_eq!(p.routes, ["0.0.0.0/0", "::/0"]);
        assert_eq!(p.dns, ["1.1.1.1"]);
        assert_eq!(p.excludes.len(), MAX_EXCLUDES);
        assert!(plan(&cfg, false).excludes.is_empty());
    }
}

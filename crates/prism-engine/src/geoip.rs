//! Russian IPv4 networks for smart routing: traffic to them leaves through
//! the physical interface instead of the tunnel. The list is geoip:ru from
//! Loyalsoldier/geoip, refreshed with `scripts/update-geoip.sh`.

use std::net::Ipv4Addr;

const RU_V4: &str = include_str!("../data/geoip-ru-v4.txt");

/// (network, prefix length) pairs, already collapsed.
pub fn russia_v4() -> Vec<(Ipv4Addr, u8)> {
    RU_V4
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (a, p) = l.split_once('/')?;
            Some((a.parse().ok()?, p.parse().ok().filter(|p| *p <= 32)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains(nets: &[(Ipv4Addr, u8)], ip: Ipv4Addr) -> bool {
        nets.iter().any(|&(n, p)| {
            let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
            u32::from(ip) & mask == u32::from(n)
        })
    }

    #[test]
    fn list_is_sane() {
        let nets = russia_v4();
        assert!(nets.len() > 5000, "{} networks", nets.len());
        assert!(contains(&nets, "77.88.55.88".parse().unwrap())); // yandex.ru
        assert!(contains(&nets, "87.240.132.67".parse().unwrap())); // vk.com
        assert!(!contains(&nets, "8.8.8.8".parse().unwrap()));
        assert!(!contains(&nets, "192.168.1.1".parse().unwrap()));
        // every entry is a proper network address
        for &(n, p) in &nets {
            let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
            assert_eq!(u32::from(n) & !mask, 0, "{n}/{p}");
        }
    }
}

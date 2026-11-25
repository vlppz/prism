//! wg-quick style `.conf` parser.
//!
//! Accepts plain WireGuard configs and AmneziaWG ones (`Jc`, `Jmin`, `Jmax`,
//! `S1`–`S4`, `H1`–`H4`, `I1`–`I5`). A plain config gets the bypass profile
//! that gets plain WireGuard servers past DPI (see [`Awg::bypass_default`]).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;

use crate::ipacket::IPacket;

/// A CIDR block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = a.trim().parse().with_context(|| format!("\"{}\" isn't a valid IP address", a.trim()))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.trim().parse::<u8>().ok().filter(|p| *p <= max).with_context(|| format!("\"{s}\" has an invalid prefix length"))?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }

    pub fn is_default(&self) -> bool {
        self.prefix == 0
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// A header range for AmneziaWG `H1`–`H4` (a single value or `lo-hi`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HRange {
    pub lo: u32,
    pub hi: u32,
}

impl HRange {
    pub const fn one(v: u32) -> Self {
        Self { lo: v, hi: v }
    }
    pub fn contains(&self, v: u32) -> bool {
        (self.lo..=self.hi).contains(&v)
    }
    fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let r = match s.split_once('-') {
            Some((a, b)) => Self { lo: a.trim().parse()?, hi: b.trim().parse()? },
            None => Self::one(s.parse()?),
        };
        if r.lo > r.hi {
            bail!("\"{s}\" isn't a valid header range");
        }
        Ok(r)
    }
}

/// Obfuscation parameters (AmneziaWG semantics).
#[derive(Debug, Clone)]
pub struct Awg {
    /// Junk packets sent before every handshake initiation.
    pub jc: u16,
    pub jmin: u16,
    pub jmax: u16,
    /// Random padding prepended to init / response / cookie / data packets.
    /// Must match the server; plain WireGuard = 0.
    pub s: [u16; 4],
    /// Message type headers. Plain WireGuard = 1, 2, 3, 4.
    pub h: [HRange; 4],
    /// Signature packets (`I1`–`I5`) sent before the junk.
    pub i: Vec<IPacket>,
}

/// DNS query for example.com: the packet that made the difference on TSPU.
pub const DNS_I1: &str = "<r 2><b 0x01000001000000000000076578616d706c6503636f6d0000010001>";

impl Awg {
    /// The recipe that worked against TSPU with a plain WireGuard server.
    pub fn bypass_default() -> Self {
        Self {
            jc: 8,
            jmin: 100,
            jmax: 900,
            s: [0; 4],
            h: [HRange::one(1), HRange::one(2), HRange::one(3), HRange::one(4)],
            i: vec![IPacket::parse(DNS_I1).expect("built-in I1")],
        }
    }

    /// Heavier profile used after a stall on the same server.
    pub fn escalate(&mut self) {
        self.jc = self.jc.max(16);
        self.jmin = self.jmin.max(200);
        self.jmax = self.jmax.max(1200);
    }

    /// No junk at all (control experiments only).
    pub fn disable_junk(&mut self) {
        self.jc = 0;
        self.i.clear();
    }

    /// Whether packets on the wire are plain WireGuard (no S/H changes).
    pub fn is_vanilla_framing(&self) -> bool {
        self.s == [0; 4] && self.h == [HRange::one(1), HRange::one(2), HRange::one(3), HRange::one(4)]
    }
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub public_key: [u8; 32],
    pub preshared_key: Option<[u8; 32]>,
    /// `host:port` as written (resolved right before connecting).
    pub endpoint: String,
    pub allowed_ips: Vec<Cidr>,
    pub keepalive: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct WgConfig {
    pub private_key: [u8; 32],
    pub addresses: Vec<Cidr>,
    pub dns: Vec<IpAddr>,
    pub mtu: Option<u16>,
    pub awg: Awg,
    pub peer: Peer,
}

impl WgConfig {
    pub fn endpoint_host(&self) -> &str {
        host_of(&self.peer.endpoint)
    }
}

fn host_of(endpoint: &str) -> &str {
    if let Some(rest) = endpoint.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    endpoint.rsplit_once(':').map(|(h, _)| h).unwrap_or(endpoint)
}

fn key(value: &str, what: &str) -> Result<[u8; 32]> {
    let v = value.trim();
    if v.starts_with('<') || v.is_empty() {
        bail!("this config has a placeholder instead of the {what}. Get a config with the key filled in from your VPN provider");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(v)
        .map_err(|_| anyhow!("the {what} isn't a valid WireGuard key"))?;
    bytes.try_into().map_err(|_| anyhow!("the {what} isn't a valid WireGuard key"))
}

fn list(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// Parses a `.conf` file. `bypass` adds the default bypass profile when the
/// config has no junk settings of its own.
pub fn parse(text: &str) -> Result<WgConfig> {
    #[derive(PartialEq)]
    enum Sec {
        None,
        Interface,
        Peer,
    }
    let mut sec = Sec::None;
    let mut peers = 0;

    let mut private_key = None;
    let mut addresses = Vec::new();
    let mut dns = Vec::new();
    let mut mtu = None;
    let mut awg = Awg::bypass_default();
    let mut junk_given = false;
    let mut i_given: Vec<(usize, IPacket)> = Vec::new();

    let mut public_key = None;
    let mut preshared_key = None;
    let mut endpoint = None;
    let mut allowed_ips = Vec::new();
    let mut keepalive = None;

    for (n, raw) in text.lines().enumerate() {
        let line = raw.split(['#', ';']).next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            match line.to_ascii_lowercase().as_str() {
                "[interface]" => sec = Sec::Interface,
                "[peer]" => {
                    peers += 1;
                    if peers > 1 {
                        bail!("this config has several [Peer] sections. Prism supports one server per config");
                    }
                    sec = Sec::Peer;
                }
                other => bail!("line {}: unknown section {other}", n + 1),
            }
            continue;
        }
        if sec == Sec::None {
            // text before any [Interface]/[Peer] header: not a WireGuard config at all
            bail!("this doesn't look like a WireGuard config");
        }
        let Some((k, v)) = line.split_once('=') else {
            bail!("line {}: expected \"Key = Value\"", n + 1);
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        let num = |v: &str| v.parse::<u16>().with_context(|| format!("line {}: {k} must be a number", n + 1));
        match sec {
            Sec::Interface => match k.as_str() {
                "privatekey" => private_key = Some(key(v, "private key")?),
                "address" => {
                    for a in list(v) {
                        addresses.push(Cidr::parse(a)?);
                    }
                }
                // DNS may also list search domains; keep only IPs.
                "dns" => dns.extend(list(v).filter_map(|d| d.parse::<IpAddr>().ok())),
                "mtu" => mtu = Some(num(v)?),
                "jc" => {
                    awg.jc = num(v)?;
                    junk_given = true;
                }
                "jmin" => {
                    awg.jmin = num(v)?;
                    junk_given = true;
                }
                "jmax" => {
                    awg.jmax = num(v)?;
                    junk_given = true;
                }
                "s1" | "s2" | "s3" | "s4" => awg.s[(k.as_bytes()[1] - b'1') as usize] = num(v)?,
                "h1" | "h2" | "h3" | "h4" => {
                    awg.h[(k.as_bytes()[1] - b'1') as usize] =
                        HRange::parse(v).with_context(|| format!("line {}: invalid {k}", n + 1))?
                }
                "i1" | "i2" | "i3" | "i4" | "i5" => {
                    if !v.is_empty() {
                        let p = IPacket::parse(v).with_context(|| format!("line {}: invalid {k}", n + 1))?;
                        i_given.push(((k.as_bytes()[1] - b'1') as usize, p));
                    }
                }
                // wg-quick / AWG keys we accept and ignore.
                "listenport" | "table" | "preup" | "postup" | "predown" | "postdown" | "saveconfig" | "fwmark"
                | "j1" | "j2" | "j3" | "itime" => {}
                _ => {}
            },
            Sec::Peer => match k.as_str() {
                "publickey" => public_key = Some(key(v, "server public key")?),
                "presharedkey" => preshared_key = Some(key(v, "preshared key")?),
                "endpoint" => endpoint = Some(v.to_string()),
                "allowedips" => {
                    for a in list(v) {
                        allowed_ips.push(Cidr::parse(a)?);
                    }
                }
                "persistentkeepalive" => {
                    keepalive = match v {
                        "off" | "0" => None,
                        v => Some(num(v)?),
                    }
                }
                _ => {}
            },
            Sec::None => unreachable!("checked above"),
        }
    }

    // Config's own I-packets replace the built-in one; so do its own junk settings.
    if !i_given.is_empty() {
        i_given.sort_by_key(|(i, _)| *i);
        awg.i = i_given.into_iter().map(|(_, p)| p).collect();
    } else if junk_given {
        awg.i.clear();
    }
    if awg.jmax < awg.jmin {
        std::mem::swap(&mut awg.jmax, &mut awg.jmin);
    }
    awg.jmax = awg.jmax.min(1280);

    if sec == Sec::None {
        bail!("this doesn't look like a WireGuard config");
    }
    let private_key = private_key.context("the [Interface] section has no PrivateKey")?;
    if addresses.is_empty() {
        bail!("the [Interface] section has no Address");
    }
    if peers == 0 {
        bail!("this config has no [Peer] section");
    }
    let endpoint = endpoint.context("the [Peer] section has no Endpoint")?;
    if !endpoint.contains(':') {
        bail!("the endpoint \"{endpoint}\" has no port");
    }
    if allowed_ips.is_empty() {
        allowed_ips.push(Cidr { addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED), prefix: 0 });
        allowed_ips.push(Cidr { addr: IpAddr::V6(Ipv6Addr::UNSPECIFIED), prefix: 0 });
    }

    Ok(WgConfig {
        private_key,
        addresses,
        dns,
        mtu,
        awg,
        peer: Peer {
            public_key: public_key.context("the [Peer] section has no PublicKey")?,
            preshared_key,
            endpoint,
            allowed_ips,
            keepalive,
        },
    })
}

/// A short human name for a config: `fra.vpn.example.com` → `fra`.
pub fn suggest_name(cfg: &WgConfig, file_name: Option<&str>) -> String {
    if let Some(f) = file_name {
        let stem = f.rsplit(['/', '\\']).next().unwrap_or(f);
        let stem = stem.strip_suffix(".conf").unwrap_or(stem);
        if !stem.is_empty() {
            return stem.to_string();
        }
    }
    let host = cfg.endpoint_host();
    if host.parse::<IpAddr>().is_ok() {
        return host.to_string();
    }
    host.split('.').next().unwrap_or(host).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "YH0Yx0p7Dt3Sx0b7u0wYkBGrH+9mGgr+0X0LkM2nZ0E=";

    fn sample(extra: &str) -> String {
        format!(
            "[Interface]\nPrivateKey = {KEY}\nAddress = 10.14.0.2/16\nDNS = 162.252.172.57, 149.154.159.92\n{extra}\n\
             [Peer]\nPublicKey = {KEY}\nAllowedIPs = 0.0.0.0/0\nEndpoint = vpn.example.com:51820\n"
        )
    }

    #[test]
    fn plain_config_gets_bypass_profile() {
        let c = parse(&sample("")).unwrap();
        assert_eq!((c.awg.jc, c.awg.jmin, c.awg.jmax), (8, 100, 900));
        assert_eq!(c.awg.i.len(), 1);
        assert!(c.awg.is_vanilla_framing());
        assert_eq!(c.dns.len(), 2);
        assert_eq!(c.endpoint_host(), "vpn.example.com");
        assert_eq!(suggest_name(&c, None), "vpn");
        assert_eq!(suggest_name(&c, Some("/x/pl-waw.conf")), "pl-waw");
    }

    #[test]
    fn own_awg_settings_win() {
        let c = parse(&sample("Jc = 4\nJmin = 40\nJmax = 70\nS1 = 15\nH1 = 100-200")).unwrap();
        assert_eq!((c.awg.jc, c.awg.jmin, c.awg.jmax), (4, 40, 70));
        assert!(c.awg.i.is_empty());
        assert_eq!(c.awg.s[0], 15);
        assert_eq!(c.awg.h[0], HRange { lo: 100, hi: 200 });
        assert!(!c.awg.is_vanilla_framing());
    }

    #[test]
    fn placeholder_key_is_explained() {
        let t = sample("").replacen(KEY, "<insert_your_private_key_here>", 1);
        let e = parse(&t).unwrap_err().to_string();
        assert!(e.contains("placeholder instead of the private key"), "{e}");
    }

    #[test]
    fn non_config_is_named() {
        for text in ["%PDF-1.7\nbinary junk", "", "hello = world"] {
            assert_eq!(parse(text).unwrap_err().to_string(), "this doesn't look like a WireGuard config");
        }
    }

    #[test]
    fn two_peers_rejected() {
        let t = sample("") + &format!("[Peer]\nPublicKey = {KEY}\nEndpoint = 1.2.3.4:1\n");
        assert!(parse(&t).unwrap_err().to_string().contains("several [Peer]"));
    }

    #[test]
    fn ipv6_endpoint_host() {
        let t = sample("").replace("vpn.example.com:51820", "[2001:db8::1]:51820");
        assert_eq!(parse(&t).unwrap().endpoint_host(), "2001:db8::1");
    }

    /// Real configs in ~/Downloads, if present (keys are never printed).
    #[test]
    fn real_downloads_parse() {
        let Some(home) = std::env::var_os("HOME") else { return };
        let dir = std::path::Path::new(&home).join(std::env::var("PRISM_CONF_DIR").unwrap_or_else(|_| "Downloads".into()));
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("conf") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            match parse(&text) {
                Ok(c) => eprintln!("ok   {} → {} (jc {})", p.display(), c.peer.endpoint, c.awg.jc),
                Err(e) => eprintln!("err  {}: {e}", p.display()),
            }
        }
    }
}

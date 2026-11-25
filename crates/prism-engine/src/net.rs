//! OS network plumbing: physical interface, socket binding, routes.
//!
//! The WireGuard UDP socket is bound to the physical interface (macOS
//! `IP_BOUND_IF`, Windows `IP_UNICAST_IF`), so it never loops back into our
//! own tunnel routes and no host route for the server is needed.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::Cidr;

pub(crate) fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let mut c = Command::new(cmd);
    c.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = c.output().with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout).into_owned() } else { err.into_owned() };
        bail!("{cmd} {}: {}", args.join(" "), msg.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Interface that carries the internet (default route), as name + index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phys {
    pub name: String,
    pub index: u32,
    /// Next hop of the default route (None for point-to-point links).
    pub gateway: Option<IpAddr>,
}

/// Tunnel-style interface names used by VPN clients.
pub fn is_vpn_interface(name: &str) -> bool {
    ["utun", "tun", "wg", "ppp", "ipsec", "tap"].iter().any(|p| name.starts_with(p))
}

pub fn physical_interface() -> Result<Phys> {
    #[cfg(target_os = "macos")]
    {
        let out = run("/sbin/route", &["-n", "get", "default"]).context("no internet connection")?;
        let name = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("interface:").map(|i| i.trim().to_string()))
            .context("no internet connection")?;
        let gateway = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("gateway:").and_then(|g| g.trim().parse::<IpAddr>().ok()));
        let c = std::ffi::CString::new(name.clone())?;
        // SAFETY: valid NUL-terminated string.
        let index = unsafe { libc::if_nametoindex(c.as_ptr()) };
        if index == 0 {
            bail!("interface {name} has no index");
        }
        Ok(Phys { name, index, gateway })
    }
    #[cfg(windows)]
    {
        let out = run(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-NetRoute -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue | \
                 Where-Object { $_.InterfaceAlias -ne 'Prism' } | Sort-Object RouteMetric | \
                 Select-Object -First 1 | ForEach-Object { \"$($_.ifIndex)|$($_.NextHop)|$($_.InterfaceAlias)\" }",
            ],
        )?;
        let line = out.trim();
        let mut parts = line.splitn(3, '|');
        let (Some(idx), Some(hop), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("no internet connection");
        };
        let gateway = hop.trim().parse::<IpAddr>().ok().filter(|g| !g.is_unspecified());
        Ok(Phys { name: name.to_string(), index: idx.trim().parse().context("bad interface index")?, gateway })
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        bail!("unsupported platform")
    }
}

/// Fails if another full-tunnel VPN already owns the internet route: its
/// routes would fight ours.
pub fn check_no_other_vpn() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let out = run("/sbin/route", &["-n", "get", "1.1.1.1"]).unwrap_or_default();
        if let Some(iface) = out.lines().find_map(|l| l.trim().strip_prefix("interface:").map(|i| i.trim().to_string())) {
            if is_vpn_interface(&iface) {
                bail!("another VPN is already active ({iface}). Turn it off and try again");
            }
        }
    }
    Ok(())
}

/// Creates the UDP socket for the WireGuard link, bound to `phys` and
/// connected to the server.
pub fn link_socket(server: SocketAddr, phys: &Phys) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if server.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = sock.set_recv_buffer_size(4 << 20);
    let _ = sock.set_send_buffer_size(4 << 20);
    bind_to_interface(&sock, server.is_ipv4(), phys.index)
        .with_context(|| format!("binding to {}", phys.name))?;
    let any: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse()? } else { "[::]:0".parse()? };
    sock.bind(&any.into())?;
    sock.connect(&server.into()).with_context(|| format!("connecting UDP to {server}"))?;
    sock.set_nonblocking(true)?;
    Ok(sock.into())
}

fn bind_to_interface(sock: &socket2::Socket, v4: bool, index: u32) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let idx = std::num::NonZeroU32::new(index).context("interface index 0")?;
        if v4 {
            sock.bind_device_by_index_v4(Some(idx))?;
        } else {
            sock.bind_device_by_index_v6(Some(idx))?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IPPROTO_IPV6, SOCKET};
        const IP_UNICAST_IF: i32 = 31;
        const IPV6_UNICAST_IF: i32 = 31;
        let s = sock.as_raw_socket() as SOCKET;
        // IPv4 wants the index in network byte order, IPv6 in host order.
        let (level, opt, val) =
            if v4 { (IPPROTO_IP, IP_UNICAST_IF, index.to_be()) } else { (IPPROTO_IPV6, IPV6_UNICAST_IF, index) };
        // SAFETY: valid socket and a 4-byte option value.
        let rc = unsafe { setsockopt(s, level, opt, &val as *const u32 as *const u8, 4) };
        if rc != 0 {
            bail!("setsockopt unicast_if: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (sock, v4, index);
        Ok(())
    }
}

/// A /32 route to the WireGuard server via the real gateway, so the
/// tunnel's own routes can never capture its packets. (On macOS a socket
/// bound to the *primary* interface is still checked against the main table,
/// where our 0/1 + 128/1 routes win: "Network is unreachable".)
pub struct HostRoute {
    ip: IpAddr,
    phys: Phys,
}

impl HostRoute {
    pub fn install(ip: IpAddr, phys: &Phys) -> Result<Self> {
        let r = Self { ip, phys: phys.clone() };
        r.remove_quiet();
        let ip_s = ip.to_string();
        #[cfg(target_os = "macos")]
        {
            let fam = if ip.is_ipv4() { "-inet" } else { "-inet6" };
            match phys.gateway.filter(|g| g.is_ipv4() == ip.is_ipv4()) {
                Some(gw) => run("/sbin/route", &["-q", "-n", "add", fam, "-host", &ip_s, &gw.to_string()])?,
                None => run("/sbin/route", &["-q", "-n", "add", fam, "-host", &ip_s, "-interface", &phys.name])?,
            };
        }
        #[cfg(windows)]
        {
            let hop = phys.gateway.filter(|g| g.is_ipv4() == ip.is_ipv4()).map(|g| g.to_string());
            let (fam, prefix, hop) = if ip.is_ipv4() {
                ("ipv4", format!("{ip_s}/32"), hop.unwrap_or_else(|| "0.0.0.0".into()))
            } else {
                ("ipv6", format!("{ip_s}/128"), hop.unwrap_or_else(|| "::".into()))
            };
            run(
                "netsh",
                &[
                    "interface",
                    fam,
                    "add",
                    "route",
                    &format!("prefix={prefix}"),
                    &format!("interface={}", phys.index),
                    &format!("nexthop={hop}"),
                    "metric=0",
                    "store=active",
                ],
            )?;
        }
        let _ = ip_s;
        Ok(r)
    }

    fn remove_quiet(&self) {
        let ip_s = self.ip.to_string();
        #[cfg(target_os = "macos")]
        {
            let fam = if self.ip.is_ipv4() { "-inet" } else { "-inet6" };
            let _ = run("/sbin/route", &["-q", "-n", "delete", fam, "-host", &ip_s]);
        }
        #[cfg(windows)]
        {
            let (fam, prefix) =
                if self.ip.is_ipv4() { ("ipv4", format!("{ip_s}/32")) } else { ("ipv6", format!("{ip_s}/128")) };
            let _ = run(
                "netsh",
                &[
                    "interface",
                    fam,
                    "delete",
                    "route",
                    &format!("prefix={prefix}"),
                    &format!("interface={}", self.phys.index),
                    "store=active",
                ],
            );
        }
        let _ = (ip_s, &self.phys);
    }

    pub fn remove(self) {
        self.remove_quiet();
    }
}

/// `0.0.0.0/0` can't beat the existing default route, so a full tunnel is
/// split into two halves (same for IPv6).
pub fn expand_routes(allowed: &[Cidr]) -> Vec<Cidr> {
    let mut out = Vec::new();
    for c in allowed {
        if c.is_default() {
            let halves: [&str; 2] = if c.addr.is_ipv4() { ["0.0.0.0/1", "128.0.0.0/1"] } else { ["::/1", "8000::/1"] };
            out.extend(halves.iter().map(|h| Cidr::parse(h).expect("static")));
        } else {
            out.push(*c);
        }
    }
    out.dedup();
    out
}

/// Routes we installed, removed again by [`Routes::teardown`].
#[derive(Debug, Default)]
pub struct Routes {
    installed: Vec<Cidr>,
    tun_name: String,
    tun_index: u32,
}

impl Routes {
    pub fn install(tun_name: &str, tun_index: u32, routes: &[Cidr]) -> Result<Self> {
        let mut r = Self { installed: Vec::new(), tun_name: tun_name.to_string(), tun_index };
        for c in routes {
            match r.add(c) {
                Ok(()) => r.installed.push(*c),
                // IPv6 may be unavailable on this machine; not fatal.
                Err(e) if c.addr.is_ipv6() => eprintln!("IPv6 route {c} not installed: {e:#}"),
                Err(e) => {
                    r.teardown();
                    return Err(e.context(format!("adding route {c}")));
                }
            }
        }
        Ok(r)
    }

    fn add(&self, c: &Cidr) -> Result<()> {
        let cidr = c.to_string();
        #[cfg(target_os = "macos")]
        {
            let fam = if c.addr.is_ipv4() { "-inet" } else { "-inet6" };
            // a stale route from a crashed session would make `add` fail
            let _ = run("/sbin/route", &["-q", "-n", "delete", fam, "-net", &cidr]);
            run("/sbin/route", &["-q", "-n", "add", fam, "-net", &cidr, "-interface", &self.tun_name]).map(drop)
        }
        #[cfg(windows)]
        {
            let (fam, hop) = if c.addr.is_ipv4() { ("ipv4", "0.0.0.0") } else { ("ipv6", "::") };
            run(
                "netsh",
                &[
                    "interface",
                    fam,
                    "add",
                    "route",
                    &format!("prefix={cidr}"),
                    &format!("interface={}", self.tun_index),
                    &format!("nexthop={hop}"),
                    "metric=0",
                    "store=active",
                ],
            )
            .map(drop)
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = cidr;
            bail!("unsupported platform")
        }
    }

    pub fn teardown(&mut self) {
        for c in self.installed.drain(..) {
            let cidr = c.to_string();
            #[cfg(target_os = "macos")]
            {
                let fam = if c.addr.is_ipv4() { "-inet" } else { "-inet6" };
                let _ = run("/sbin/route", &["-q", "-n", "delete", fam, "-net", &cidr, "-interface", &self.tun_name]);
            }
            #[cfg(windows)]
            {
                let fam = if c.addr.is_ipv4() { "ipv4" } else { "ipv6" };
                let _ = run(
                    "netsh",
                    &[
                        "interface",
                        fam,
                        "delete",
                        "route",
                        &format!("prefix={cidr}"),
                        &format!("interface={}", self.tun_index),
                        "store=active",
                    ],
                );
            }
            let _ = cidr;
        }
        let _ = (&self.tun_name, self.tun_index);
    }
}

/// Smart routing: networks that leave through the physical gateway instead
/// of the tunnel. There are thousands of them, so they go straight through
/// the OS routing API (macOS routing socket, Windows IP Helper) rather than
/// one `route` process each; installing ~13k takes well under a second.
///
/// A marker file in the state directory remembers the gateway they point at,
/// so a session that died without cleaning up is undone by [`clear_stale_direct`].
pub struct DirectRoutes {
    nets: Vec<(Ipv4Addr, u8)>,
    gateway: Ipv4Addr,
    index: u32,
    marker: PathBuf,
}

const DIRECT_MARKER: &str = "direct-routes";

impl DirectRoutes {
    pub fn install(nets: Vec<(Ipv4Addr, u8)>, phys: &Phys, state_dir: &Path) -> Result<Self> {
        let Some(IpAddr::V4(gateway)) = phys.gateway else {
            bail!("{} has no IPv4 gateway", phys.name);
        };
        let marker = state_dir.join(DIRECT_MARKER);
        let _ = std::fs::create_dir_all(state_dir);
        std::fs::write(&marker, format!("{gateway} {}\n", phys.index)).context("writing the routes marker")?;
        let r = Self { nets, gateway, index: phys.index, marker };
        let mut table = sys::Table::open()?;
        for (i, &(net, prefix)) in r.nets.iter().enumerate() {
            if let Err(e) = table.add(net, prefix, gateway, r.index) {
                // undo the ones already in
                for &(n, p) in &r.nets[..i] {
                    let _ = table.delete(n, p, gateway, r.index);
                }
                let _ = std::fs::remove_file(&r.marker);
                return Err(e.context(format!("adding route {net}/{prefix}")));
            }
        }
        Ok(r)
    }

    pub fn len(&self) -> usize {
        self.nets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    pub fn remove(self) {
        if let Ok(mut table) = sys::Table::open() {
            for &(n, p) in &self.nets {
                let _ = table.delete(n, p, self.gateway, self.index);
            }
        }
        let _ = std::fs::remove_file(&self.marker);
    }
}

/// Removes direct routes a crashed session left behind (they would keep
/// pointing at an old gateway).
pub fn clear_stale_direct(state_dir: &Path, nets: &[(Ipv4Addr, u8)]) {
    let marker = state_dir.join(DIRECT_MARKER);
    let Ok(text) = std::fs::read_to_string(&marker) else { return };
    let mut it = text.split_whitespace();
    if let (Some(Ok(gateway)), Some(Ok(index))) = (it.next().map(str::parse::<Ipv4Addr>), it.next().map(str::parse::<u32>)) {
        if let Ok(mut table) = sys::Table::open() {
            for &(n, p) in nets {
                let _ = table.delete(n, p, gateway, index);
            }
        }
    }
    let _ = std::fs::remove_file(&marker);
}

#[cfg(target_os = "macos")]
mod sys {
    //! PF_ROUTE messages: rt_msghdr followed by destination, gateway and
    //! netmask sockaddrs (each sockaddr_in is 16 bytes, already aligned).
    use std::io;
    use std::mem::size_of;
    use std::net::Ipv4Addr;

    use anyhow::Result;

    #[repr(C)]
    struct Msg {
        hdr: libc::rt_msghdr,
        dst: libc::sockaddr_in,
        gw: libc::sockaddr_in,
        mask: libc::sockaddr_in,
    }

    pub struct Table {
        fd: libc::c_int,
        seq: libc::c_int,
    }

    fn sin(a: Ipv4Addr) -> libc::sockaddr_in {
        libc::sockaddr_in {
            sin_len: size_of::<libc::sockaddr_in>() as u8,
            sin_family: libc::AF_INET as u8,
            sin_port: 0,
            sin_addr: libc::in_addr { s_addr: u32::from(a).to_be() },
            sin_zero: [0; 8],
        }
    }

    impl Table {
        pub fn open() -> Result<Self> {
            // SAFETY: plain socket call; the fd is owned by Table and closed on drop.
            let fd = unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, libc::AF_UNSPEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error().into());
            }
            // we never read the socket: don't queue copies of our own messages
            let off: libc::c_int = 0;
            // SAFETY: valid fd and option buffer.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_USELOOPBACK,
                    (&off as *const libc::c_int).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
            Ok(Self { fd, seq: 0 })
        }

        fn send(&mut self, kind: libc::c_int, net: Ipv4Addr, prefix: u8, gw: Ipv4Addr) -> io::Result<()> {
            self.seq += 1;
            // SAFETY: Msg is plain old data; all-zero is a valid value.
            let mut m: Msg = unsafe { std::mem::zeroed() };
            m.hdr.rtm_msglen = size_of::<Msg>() as u16;
            m.hdr.rtm_version = libc::RTM_VERSION as u8;
            m.hdr.rtm_type = kind as u8;
            m.hdr.rtm_flags = libc::RTF_UP | libc::RTF_GATEWAY | libc::RTF_STATIC;
            m.hdr.rtm_addrs = libc::RTA_DST | libc::RTA_GATEWAY | libc::RTA_NETMASK;
            m.hdr.rtm_seq = self.seq;
            m.dst = sin(net);
            m.gw = sin(gw);
            m.mask = sin(Ipv4Addr::from(if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix)) }));
            // SAFETY: writes exactly one message from a valid buffer.
            let n = unsafe { libc::write(self.fd, (&m as *const Msg).cast(), size_of::<Msg>()) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub fn add(&mut self, net: Ipv4Addr, prefix: u8, gw: Ipv4Addr, _index: u32) -> Result<()> {
            match self.send(libc::RTM_ADD, net, prefix, gw) {
                // left over from an earlier session: point it at today's gateway
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(self.send(libc::RTM_CHANGE, net, prefix, gw)?),
                r => Ok(r?),
            }
        }

        pub fn delete(&mut self, net: Ipv4Addr, prefix: u8, gw: Ipv4Addr, _index: u32) -> Result<()> {
            Ok(self.send(libc::RTM_DELETE, net, prefix, gw)?)
        }
    }

    impl Drop for Table {
        fn drop(&mut self) {
            // SAFETY: we own the fd.
            unsafe { libc::close(self.fd) };
        }
    }
}

#[cfg(windows)]
mod sys {
    //! IP Helper: CreateIpForwardEntry2 / DeleteIpForwardEntry2 (active store
    //! only, gone after a reboot).
    use std::net::Ipv4Addr;

    use anyhow::{bail, Result};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        CreateIpForwardEntry2, DeleteIpForwardEntry2, InitializeIpForwardEntry, MIB_IPFORWARD_ROW2,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, MIB_IPPROTO_NETMGMT};

    const ERROR_OBJECT_ALREADY_EXISTS: u32 = 5010;
    const ERROR_NOT_FOUND: u32 = 1168;

    pub struct Table;

    fn row(net: Ipv4Addr, prefix: u8, gw: Ipv4Addr, index: u32) -> MIB_IPFORWARD_ROW2 {
        // SAFETY: MIB_IPFORWARD_ROW2 is plain data; InitializeIpForwardEntry fills the defaults.
        unsafe {
            let mut r: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
            InitializeIpForwardEntry(&mut r);
            r.InterfaceIndex = index;
            r.DestinationPrefix.Prefix.si_family = AF_INET;
            r.DestinationPrefix.Prefix.Ipv4.sin_family = AF_INET;
            r.DestinationPrefix.Prefix.Ipv4.sin_addr.S_un.S_addr = u32::from(net).to_be();
            r.DestinationPrefix.PrefixLength = prefix;
            r.NextHop.si_family = AF_INET;
            r.NextHop.Ipv4.sin_family = AF_INET;
            r.NextHop.Ipv4.sin_addr.S_un.S_addr = u32::from(gw).to_be();
            r.Metric = 0;
            r.Protocol = MIB_IPPROTO_NETMGMT;
            r
        }
    }

    impl Table {
        pub fn open() -> Result<Self> {
            Ok(Self)
        }

        pub fn add(&mut self, net: Ipv4Addr, prefix: u8, gw: Ipv4Addr, index: u32) -> Result<()> {
            let r = row(net, prefix, gw, index);
            // SAFETY: valid, initialised row.
            match unsafe { CreateIpForwardEntry2(&r) } {
                0 | ERROR_OBJECT_ALREADY_EXISTS => Ok(()),
                e => bail!("CreateIpForwardEntry2 failed ({e})"),
            }
        }

        pub fn delete(&mut self, net: Ipv4Addr, prefix: u8, gw: Ipv4Addr, index: u32) -> Result<()> {
            let r = row(net, prefix, gw, index);
            // SAFETY: valid, initialised row.
            match unsafe { DeleteIpForwardEntry2(&r) } {
                0 | ERROR_NOT_FOUND => Ok(()),
                e => bail!("DeleteIpForwardEntry2 failed ({e})"),
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod sys {
    use std::net::Ipv4Addr;

    use anyhow::{bail, Result};

    pub struct Table;

    impl Table {
        pub fn open() -> Result<Self> {
            bail!("unsupported platform")
        }
        pub fn add(&mut self, _: Ipv4Addr, _: u8, _: Ipv4Addr, _: u32) -> Result<()> {
            bail!("unsupported platform")
        }
        pub fn delete(&mut self, _: Ipv4Addr, _: u8, _: Ipv4Addr, _: u32) -> Result<()> {
            bail!("unsupported platform")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn route_message_layout() {
        // the kernel rejects messages whose header size it doesn't expect
        assert_eq!(std::mem::size_of::<libc::rt_msghdr>(), 92);
        assert_eq!(std::mem::size_of::<libc::sockaddr_in>(), 16);
    }

    #[test]
    fn full_tunnel_is_split() {
        let r = expand_routes(&[Cidr::parse("0.0.0.0/0").unwrap(), Cidr::parse("::/0").unwrap()]);
        let s: Vec<String> = r.iter().map(|c| c.to_string()).collect();
        assert_eq!(s, ["0.0.0.0/1", "128.0.0.0/1", "::/1", "8000::/1"]);
        let r = expand_routes(&[Cidr::parse("10.0.0.0/8").unwrap()]);
        assert_eq!(r[0].to_string(), "10.0.0.0/8");
    }
}

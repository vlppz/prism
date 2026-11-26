//! The tunnel: TUN device ⇄ boringtun ⇄ UDP socket, with the bypass hook.
//!
//! Layout of a session:
//! * set up once: resolve server, TUN device, routes, DNS;
//! * a *link* (UDP socket + WireGuard state + 3 tasks) runs on top of it.
//!   When TSPU stalls the flow, the link is thrown away and a new one starts
//!   from a fresh source port (TSPU judges per flow), with heavier junk.
//! * torn down in reverse order on stop or error.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use boringtun::noise::errors::WireGuardError;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519;
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tun_rs::AsyncDevice;
#[cfg(not(target_os = "android"))]
use tun_rs::DeviceBuilder;

use crate::config::{Awg, WgConfig};
use crate::net::{self, Phys};
use crate::obfs::{self, Obfs};
use crate::proto::{Event, Phase};
use crate::dns;

pub struct Options {
    /// Control experiments only: no junk / signature packets.
    pub no_junk: bool,
    /// Where crash-recovery state (DNS backup) lives.
    pub state_dir: PathBuf,
    /// Windows: full path of wintun.dll.
    pub wintun_path: Option<String>,
    /// Smart routing: Russian networks go straight out, not through the tunnel.
    pub smart: bool,
    /// Android: the TUN descriptor from `VpnService.Builder.establish()`. The
    /// system already set its addresses, routes and DNS. Owned here, so the VPN
    /// interface goes away with the session whichever way it ends.
    #[cfg(target_os = "android")]
    pub tun_fd: std::os::fd::OwnedFd,
}

pub type Events = mpsc::UnboundedSender<Event>;

const BUF: usize = 65536;
/// No handshake reply for this long → tell the user the server looks blocked.
const UNREACHABLE_AFTER: Duration = Duration::from_secs(10);
/// …and for this long → retry from a new source port.
const RELINK_AFTER: Duration = Duration::from_secs(15);
/// WireGuard rejects sessions older than 180 s; past this the link is dead.
const DEAD_AFTER: Duration = Duration::from_secs(200);
/// Stall = we sent at least this much in the window and got nothing back.
const STALL_WINDOW: usize = 8;
const STALL_TX_BYTES: u64 = 4000;

fn emit(ev: &Events, e: Event) {
    let _ = ev.send(e);
}

fn log(ev: &Events, line: impl Into<String>) {
    let line = line.into();
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    eprintln!("{h:02}:{m:02}:{s:02}Z {line}");
    emit(ev, Event::Log { line });
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.context("internal error")?
}

async fn resolve(endpoint: &str) -> Result<Vec<SocketAddr>> {
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host(endpoint)
        .await
        .with_context(|| format!("couldn't look up the server {endpoint}. Check your internet connection"))?
        .collect();
    // IPv4 first: it's what TSPU-tested setups use and works on every network.
    addrs.sort_by_key(|a| !a.is_ipv4());
    addrs.dedup();
    if addrs.is_empty() {
        bail!("the server {endpoint} has no IP address");
    }
    Ok(addrs)
}

/// Runs a session until `stop` flips to true. Always cleans up after itself.
pub async fn run(cfg: WgConfig, opts: Options, ev: Events, mut stop: watch::Receiver<bool>) -> Result<()> {
    emit(&ev, Event::Phase { phase: Phase::Connecting });
    // A previous session may have died before restoring DNS or removing its
    // direct routes.
    {
        let state = opts.state_dir.clone();
        blocking(move || {
            dns::restore(&state);
            net::clear_stale_direct(&state, &crate::geoip::russia_v4());
            Ok(())
        })
        .await?;
    }
    blocking(net::check_no_other_vpn).await?;

    let servers = resolve(&cfg.peer.endpoint).await?;
    let server = servers[0];
    let phys = blocking(net::physical_interface).await?;
    log(&ev, format!("server {server} via {}", phys.name));

    let tun = Arc::new(create_tun(&cfg, &opts).context("couldn't create the VPN network interface")?);
    let (tun_name, tun_index) = tun_ids(&tun).context("couldn't create the VPN network interface")?;
    log(&ev, format!("interface {tun_name}"));

    let has_v6 = cfg.addresses.iter().any(|a| a.addr.is_ipv6());
    let mut allowed = cfg.peer.allowed_ips.clone();
    // IPv4-only full tunnel: capture IPv6 too and drop it in `tun_to_net`.
    // Otherwise apps prefer IPv6, go straight to the ISP and hit the same
    // blocks; dropped, they fall back to IPv4 through the tunnel.
    let full_v4 = allowed.iter().any(|c| c.is_default() && c.addr.is_ipv4());
    if full_v4 && !has_v6 && !allowed.iter().any(|c| c.is_default() && c.addr.is_ipv6()) {
        allowed.push(crate::config::Cidr::parse("::/0")?);
    }
    let routes = net::expand_routes(&allowed);
    let name = tun_name.clone();
    let mut routes =
        blocking(move || net::Routes::install(&name, tun_index, &routes)).await.context("couldn't set up the VPN routes")?;

    let dns_servers: Vec<IpAddr> = cfg.dns.iter().copied().filter(|d| d.is_ipv4() || has_v6).collect();
    {
        let (state, servers) = (opts.state_dir.clone(), dns_servers.clone());
        let _ = blocking(move || {
            dns::apply(&state, &servers);
            Ok(())
        })
        .await;
    }
    #[cfg(windows)]
    if !dns_servers.is_empty() {
        if let Err(e) = tun.set_dns_servers(&dns_servers) {
            log(&ev, format!("could not set DNS: {e}"));
        }
    }

    if servers.len() > 1 {
        log(&ev, format!("{} addresses for {}", servers.len(), cfg.endpoint_host()));
    }
    let result = supervise(&cfg, &opts, servers, phys, tun.clone(), has_v6, &ev, &mut stop).await;

    let state = opts.state_dir.clone();
    let _ = blocking(move || {
        routes.teardown();
        dns::restore(&state);
        Ok(())
    })
    .await;
    drop(tun);
    result
}

#[cfg(target_os = "android")]
fn create_tun(_cfg: &WgConfig, opts: &Options) -> Result<AsyncDevice> {
    use std::os::fd::IntoRawFd;
    let fd = opts.tun_fd.try_clone()?.into_raw_fd();
    // SAFETY: a fresh duplicate that nothing else uses.
    Ok(unsafe { AsyncDevice::from_fd(fd) }?)
}

#[cfg(not(target_os = "android"))]
fn create_tun(cfg: &WgConfig, opts: &Options) -> Result<AsyncDevice> {
    let mut b = DeviceBuilder::new().mtu(cfg.mtu.unwrap_or(1420));
    if let Some(a) = cfg.addresses.iter().find(|a| a.addr.is_ipv4()) {
        let IpAddr::V4(v4) = a.addr else { unreachable!() };
        b = b.ipv4(v4, a.prefix, None);
    }
    for a in cfg.addresses.iter().filter(|a| a.addr.is_ipv6()) {
        let IpAddr::V6(v6) = a.addr else { unreachable!() };
        b = b.ipv6(v6, a.prefix);
    }
    #[cfg(target_os = "macos")]
    {
        // we install routes ourselves
        b = b.associate_route(false);
    }
    #[cfg(windows)]
    {
        b = b.name("Prism");
        if let Some(p) = &opts.wintun_path {
            b = b.wintun_file(p.clone());
        }
    }
    let _ = opts;
    let dev = b.build_async()?;
    #[cfg(windows)]
    {
        // low metric: Windows prefers this adapter's DNS
        let _ = dev.set_metric(1);
    }
    Ok(dev)
}

fn tun_ids(tun: &AsyncDevice) -> Result<(String, u32)> {
    #[cfg(target_os = "android")]
    let name = {
        let _ = tun;
        "the VPN interface".to_string()
    };
    #[cfg(not(target_os = "android"))]
    let name = tun.name().context("reading interface name")?;
    #[cfg(windows)]
    let index = tun.if_index().context("reading interface index")?;
    #[cfg(not(windows))]
    let index = 0;
    Ok((name, index))
}

enum Outcome {
    Stop,
    Stalled,
    Unreachable,
    Dead,
    NetworkChanged,
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    cfg: &WgConfig,
    opts: &Options,
    servers: Vec<SocketAddr>,
    mut phys: Phys,
    tun: Arc<AsyncDevice>,
    has_v6: bool,
    ev: &Events,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    let mut awg = cfg.awg.clone();
    if opts.no_junk {
        awg.disable_junk();
        log(ev, "junk packets disabled (control mode)");
    }
    let mut level = 0u8;
    let mut totals = (0u64, 0u64); // rx, tx over all links
    let mut failures = 0u32;
    let mut pick = 0usize;
    let mut server = servers[0];
    let mut host_route = {
        let (ip, p) = (server.ip(), phys.clone());
        blocking(move || net::HostRoute::install(ip, &p)).await.context("couldn't set up the route to the server")?
    };
    let direct_nets = if opts.smart { crate::geoip::russia_v4() } else { Vec::new() };
    let mut direct = install_direct(&direct_nets, &phys, opts, ev).await;
    let mut last_phase = Phase::Connecting;
    let mut set_phase = |p: Phase| {
        if p != last_phase {
            last_phase = p;
            emit(ev, Event::Phase { phase: p });
        }
    };

    loop {
        if *stop.borrow() {
            break;
        }
        let outcome = match Link::start(cfg, &awg, server, &phys, tun.clone(), has_v6) {
            Ok(mut link) => {
                failures = 0;
                let o = watch_link(&mut link, ev, stop, totals, level, &mut set_phase).await;
                let (_, tx, rx, _, _) = link.tunn.lock().stats();
                totals.0 += rx as u64;
                totals.1 += tx as u64;
                link.stop();
                o
            }
            Err(e) => Outcome::Failed(format!("{e:#}")),
        };
        match outcome {
            Outcome::Stop => break,
            Outcome::Stalled => {
                set_phase(Phase::Stalled);
                if level == 0 && !opts.no_junk {
                    level = 1;
                    awg.escalate();
                    log(ev, format!("traffic stalled — reconnecting with heavier junk (Jc {}, {}–{} B)", awg.jc, awg.jmin, awg.jmax));
                } else {
                    log(ev, "traffic stalled — reconnecting from a new port");
                }
            }
            Outcome::Unreachable => {
                if servers.len() > 1 {
                    // the hostname has other servers; this one may be blocked or down
                    pick = (pick + 1) % servers.len();
                    let next = servers[pick];
                    log(ev, format!("no answer from {} — trying {}", server.ip(), next.ip()));
                    let (ip, p, old) = (next.ip(), phys.clone(), host_route);
                    host_route = blocking(move || {
                        old.remove();
                        net::HostRoute::install(ip, &p)
                    })
                    .await
                    .context("couldn't set up the route to the server")?;
                    server = next;
                } else {
                    log(ev, "no answer from the server — retrying from a new port");
                }
            }
            Outcome::Dead => {
                set_phase(Phase::Connecting);
                log(ev, "session expired — reconnecting");
            }
            Outcome::NetworkChanged => log(ev, "network changed — reconnecting"),
            Outcome::Failed(e) => {
                failures += 1;
                if failures >= 3 {
                    set_phase(Phase::Unreachable);
                }
                log(ev, format!("link error: {e} — retrying"))
            }
        }
        // short pause (and a chance to stop) before the next link; back off
        // while the network is failing
        let pause = Duration::from_millis(800 * u64::from(failures.clamp(1, 6)));
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(pause) => {}
        }
        if *stop.borrow() {
            break;
        }
        // the network may have changed (Wi-Fi ↔ Ethernet)
        match blocking(net::physical_interface).await {
            Ok(p) if p != phys => {
                log(ev, format!("network changed: now via {}", p.name));
                phys = p;
                let (ip, p, old) = (server.ip(), phys.clone(), host_route);
                host_route = blocking(move || {
                    old.remove();
                    net::HostRoute::install(ip, &p)
                })
                .await
                .context("couldn't set up the route to the server")?;
                if let Some(old) = direct.take() {
                    let _ = blocking(move || {
                        old.remove();
                        Ok(())
                    })
                    .await;
                }
                direct = install_direct(&direct_nets, &phys, opts, ev).await;
            }
            Ok(_) => {}
            Err(e) => log(ev, format!("{e:#}")),
        }
    }
    let _ = blocking(move || {
        host_route.remove();
        if let Some(d) = direct {
            d.remove();
        }
        Ok(())
    })
    .await;
    Ok(())
}

/// Smart routing: send Russian networks straight out through `phys`. A
/// failure only costs the feature, never the tunnel.
async fn install_direct(nets: &[(std::net::Ipv4Addr, u8)], phys: &Phys, opts: &Options, ev: &Events) -> Option<net::DirectRoutes> {
    if nets.is_empty() {
        return None;
    }
    let (n, p, dir) = (nets.to_vec(), phys.clone(), opts.state_dir.clone());
    let t = Instant::now();
    match blocking(move || net::DirectRoutes::install(n, &p, &dir)).await {
        Ok(r) => {
            log(ev, format!("smart routing: {} Russian networks go direct via {} ({} ms)", r.len(), phys.name, t.elapsed().as_millis()));
            Some(r)
        }
        Err(e) => {
            log(ev, format!("smart routing unavailable, everything goes through the tunnel: {e:#}"));
            None
        }
    }
}

async fn watch_link(
    link: &mut Link,
    ev: &Events,
    stop: &mut watch::Receiver<bool>,
    totals: (u64, u64),
    level: u8,
    set_phase: &mut impl FnMut(Phase),
) -> Outcome {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut hist: VecDeque<(u64, u64)> = VecDeque::with_capacity(STALL_WINDOW + 1);
    loop {
        tokio::select! {
            r = stop.changed() => if r.is_err() || *stop.borrow() { return Outcome::Stop },
            Some(e) = link.fail_rx.recv() => return Outcome::Failed(e),
            _ = tick.tick() => {}
        }
        if net::network_epoch() != link.epoch {
            return Outcome::NetworkChanged;
        }
        let (hs, tx, rx, _, _) = link.tunn.lock().stats();
        let (tx, rx) = (tx as u64, rx as u64);
        emit(
            ev,
            Event::Stats { rx: totals.0 + rx, tx: totals.1 + tx, handshake_age: hs.map(|d| d.as_secs()), level },
        );
        match hs {
            None => {
                let age = link.started.elapsed();
                if age > UNREACHABLE_AFTER {
                    set_phase(Phase::Unreachable);
                }
                if age > RELINK_AFTER {
                    return Outcome::Unreachable;
                }
            }
            Some(h) => {
                if h > DEAD_AFTER && dead_if_quiet(&hist, tx) {
                    return Outcome::Dead;
                }
                set_phase(Phase::Connected);
                hist.push_back((tx, rx));
                if hist.len() > STALL_WINDOW {
                    hist.pop_front();
                }
                if hist.len() == STALL_WINDOW {
                    let (tx0, rx0) = hist[0];
                    if tx.saturating_sub(tx0) >= STALL_TX_BYTES && rx == rx0 {
                        return Outcome::Stalled;
                    }
                }
            }
        }
    }
}

/// UDP socket + obfuscation; everything that leaves for the server goes here.
struct Wire {
    sock: UdpSocket,
    obfs: Obfs,
}

impl Wire {
    async fn send(&self, pkt: &[u8], scratch: &mut Vec<u8>) {
        if obfs::is_handshake_init(pkt) {
            for p in self.obfs.preamble() {
                let _ = self.sock.send(&p).await;
            }
        }
        let _ = if self.obfs.encode_into(pkt, scratch) {
            self.sock.send(scratch).await
        } else {
            self.sock.send(pkt).await
        };
    }
}

struct Link {
    tunn: Arc<Mutex<Tunn>>,
    tasks: Vec<JoinHandle<()>>,
    fail_rx: mpsc::Receiver<String>,
    started: Instant,
    /// [`net::network_epoch`] when the link started.
    epoch: u64,
}

impl Link {
    fn start(
        cfg: &WgConfig,
        awg: &Awg,
        server: SocketAddr,
        phys: &Phys,
        tun: Arc<AsyncDevice>,
        has_v6: bool,
    ) -> Result<Self> {
        let epoch = net::network_epoch();
        let sock = UdpSocket::from_std(net::link_socket(server, phys)?)?;
        let tunn = Tunn::new(
            x25519::StaticSecret::from(cfg.private_key),
            x25519::PublicKey::from(cfg.peer.public_key),
            cfg.peer.preshared_key,
            // keepalives keep NAT open and let rekeys reveal a dead link
            keepalive(cfg),
            rand::random::<u32>() >> 8,
            None,
        );
        let tunn = Arc::new(Mutex::new(tunn));
        let wire = Arc::new(Wire { sock, obfs: Obfs::new(awg.clone()) });
        let (fail_tx, fail_rx) = mpsc::channel(4);
        let server_ip = server.ip();

        let mut tasks = Vec::with_capacity(3);
        {
            let (tun, tunn, wire, fail) = (tun.clone(), tunn.clone(), wire.clone(), fail_tx.clone());
            tasks.push(tokio::spawn(async move {
                if let Err(e) = tun_to_net(tun, tunn, wire, has_v6).await {
                    let _ = fail.send(format!("{e:#}")).await;
                }
            }));
        }
        {
            let (tunn, wire) = (tunn.clone(), wire.clone());
            tasks.push(tokio::spawn(net_to_tun(tun, tunn, wire, server_ip)));
        }
        tasks.push(tokio::spawn(timers(tunn.clone(), wire)));
        Ok(Self { tunn, tasks, fail_rx, started: Instant::now(), epoch })
    }

    fn stop(self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Offset and length of `part` inside the buffer starting at `base`.
#[inline]
fn span(base: usize, part: &[u8]) -> (usize, usize) {
    (part.as_ptr() as usize - base, part.len())
}

async fn tun_to_net(tun: Arc<AsyncDevice>, tunn: Arc<Mutex<Tunn>>, wire: Arc<Wire>, has_v6: bool) -> Result<()> {
    let mut src = vec![0u8; BUF];
    let mut dst = vec![0u8; BUF + 64];
    let base = dst.as_ptr() as usize;
    let mut scratch = Vec::with_capacity(2048);
    loop {
        let n = tun.recv(&mut src).await.context("reading from the tunnel interface")?;
        if n == 0 {
            continue;
        }
        // No IPv6 inside this tunnel: drop it instead of leaking it outside.
        if !has_v6 && src[0] >> 4 == 6 {
            if let Some(reply) = icmp6_unreachable(&src[..n]) {
                // tell the app right away so it falls back to IPv4 instantly
                let _ = tun.send(&reply).await;
            }
            continue;
        }
        let out = match tunn.lock().encapsulate(&src[..n], &mut dst) {
            TunnResult::WriteToNetwork(p) => Some(span(base, p)),
            _ => None,
        };
        if let Some((o, l)) = out {
            wire.send(&dst[o..o + l], &mut scratch).await;
        }
    }
}

/// Builds an ICMPv6 "destination unreachable (no route)" reply for an IPv6
/// packet we refuse to carry. None for ICMPv6 itself and multicast.
fn icmp6_unreachable(pkt: &[u8]) -> Option<Vec<u8>> {
    if pkt.len() < 40 || pkt[0] >> 4 != 6 || pkt[6] == 58 || pkt[24] == 0xff || pkt[8..24].iter().all(|b| *b == 0) {
        return None;
    }
    let quoted = &pkt[..pkt.len().min(1280 - 48)];
    let icmp_len = 8 + quoted.len();
    let mut out = Vec::with_capacity(40 + icmp_len);
    out.extend_from_slice(&[0x60, 0, 0, 0]);
    out.extend_from_slice(&(icmp_len as u16).to_be_bytes());
    out.extend_from_slice(&[58, 64]);
    out.extend_from_slice(&pkt[24..40]); // from: the unreachable destination
    out.extend_from_slice(&pkt[8..24]); // to: the sender
    out.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // type 1 code 0, checksum, unused
    out.extend_from_slice(quoted);
    // checksum over the pseudo-header + ICMPv6 message
    let mut sum: u32 = 0;
    let mut add = |b: &[u8]| {
        for c in b.chunks(2) {
            sum += u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]));
        }
    };
    add(&out[8..40]);
    add(&(icmp_len as u32).to_be_bytes());
    add(&[0, 0, 0, 58]);
    add(&out[40..]);
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let ck = !(sum as u16);
    out[42..44].copy_from_slice(&ck.to_be_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icmp6_reply_has_valid_checksum() {
        let mut p = vec![0u8; 60];
        p[0] = 0x60;
        p[6] = 6; // TCP
        p[8] = 0x20; // src 2000::1
        p[23] = 1;
        p[24] = 0x2a; // dst 2a00::2
        p[39] = 2;
        let r = icmp6_unreachable(&p).unwrap();
        assert_eq!(r[40], 1);
        assert_eq!(&r[8..24], &p[24..40]);
        // re-summing including the checksum must give 0xffff
        let mut sum: u32 = 0;
        for c in r[8..40].chunks(2) {
            sum += u32::from(u16::from_be_bytes([c[0], c[1]]));
        }
        let len = (r.len() - 40) as u32;
        sum += (len >> 16) + (len & 0xffff) + 58;
        for c in r[40..].chunks(2) {
            sum += u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]));
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        assert_eq!(sum, 0xffff);
        // never answer ICMPv6 or multicast
        p[6] = 58;
        assert!(icmp6_unreachable(&p).is_none());
    }
}

enum Act {
    Net(usize, usize),
    Tun(usize, usize),
    Done,
}

async fn net_to_tun(tun: Arc<AsyncDevice>, tunn: Arc<Mutex<Tunn>>, wire: Arc<Wire>, server_ip: IpAddr) {
    let mut buf = vec![0u8; BUF];
    let mut dst = vec![0u8; BUF];
    let base = dst.as_ptr() as usize;
    let mut scratch = Vec::with_capacity(2048);
    loop {
        let n = match wire.sock.recv(&mut buf).await {
            Ok(n) => n,
            // e.g. ICMP port unreachable surfaced on a connected socket, or the
            // network going away; the monitor handles persistent failure.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Some(pkt) = wire.obfs.decode(&mut buf[..n]) else { continue };
        let mut input: &[u8] = pkt;
        loop {
            let act = match tunn.lock().decapsulate(Some(server_ip), input, &mut dst) {
                TunnResult::WriteToNetwork(p) => {
                    let (o, l) = span(base, p);
                    Act::Net(o, l)
                }
                TunnResult::WriteToTunnelV4(p, _) | TunnResult::WriteToTunnelV6(p, _) => {
                    let (o, l) = span(base, p);
                    Act::Tun(o, l)
                }
                _ => Act::Done,
            };
            match act {
                // handshake replies / queued packets: keep flushing
                Act::Net(o, l) => {
                    wire.send(&dst[o..o + l], &mut scratch).await;
                    input = &[];
                }
                Act::Tun(o, l) => {
                    if l > 0 {
                        let _ = tun.send(&dst[o..o + l]).await;
                    }
                    break;
                }
                Act::Done => break,
            }
        }
    }
}

async fn timers(tunn: Arc<Mutex<Tunn>>, wire: Arc<Wire>) {
    let mut dst = vec![0u8; 2048];
    let base = dst.as_ptr() as usize;
    let mut scratch = Vec::with_capacity(2048);

    // start the handshake right away instead of waiting for traffic
    let first = match tunn.lock().format_handshake_initiation(&mut dst, false) {
        TunnResult::WriteToNetwork(p) => Some(span(base, p)),
        _ => None,
    };
    if let Some((o, l)) = first {
        wire.send(&dst[o..o + l], &mut scratch).await;
    }

    let mut tick = tokio::time::interval(TIMER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let out = {
            let mut t = tunn.lock();
            let (out, expired) = match t.update_timers(&mut dst) {
                TunnResult::WriteToNetwork(p) => (Some(span(base, p)), false),
                TunnResult::Err(WireGuardError::ConnectionExpired) => (None, true),
                _ => (None, false),
            };
            if expired {
                // keep trying: a VPN client should never give up on its own
                match t.format_handshake_initiation(&mut dst, true) {
                    TunnResult::WriteToNetwork(p) => Some(span(base, p)),
                    _ => None,
                }
            } else {
                out
            }
        };
        if let Some((o, l)) = out {
            wire.send(&dst[o..o + l], &mut scratch).await;
        }
    }
}

/// Validates a config without connecting (used on import).
pub fn validate(text: &str) -> Result<WgConfig> {
    let cfg = crate::config::parse(text)?;
    if cfg.peer.endpoint.trim().is_empty() {
        bail!("empty Endpoint");
    }
    Ok(cfg)
}

/// How often WireGuard's timers are serviced (rekeys, keepalives, retries).
/// Phones wake once a second, for the battery; WireGuard needs no finer.
#[cfg(target_os = "android")]
const TIMER_TICK: Duration = Duration::from_secs(1);
#[cfg(not(target_os = "android"))]
const TIMER_TICK: Duration = Duration::from_millis(250);


/// Keepalive interval. Desktop always sends one (keeps NAT open, lets rekeys
/// reveal a dead link). On a phone each one wakes the cellular radio, which
/// then idles at high power for seconds: every 25 s that never lets it rest.
/// So phones send them only when the config asks, like WireGuard's own app.
#[cfg(target_os = "android")]
fn keepalive(cfg: &WgConfig) -> Option<u16> {
    cfg.peer.keepalive.filter(|k| *k > 0)
}
#[cfg(not(target_os = "android"))]
#[inline(always)]
fn keepalive(cfg: &WgConfig) -> Option<u16> {
    Some(cfg.peer.keepalive.unwrap_or(25))
}

/// An old handshake means a dead link only if something tried to go out.
/// Without keepalives an idle phone's handshake simply ages, and relinking
/// then (junk, a fresh handshake) would only wake the radio for nothing.
#[cfg(target_os = "android")]
fn dead_if_quiet(hist: &VecDeque<(u64, u64)>, tx: u64) -> bool {
    hist.front().map_or(true, |&(tx0, _)| tx > tx0)
}
#[cfg(not(target_os = "android"))]
#[inline(always)]
fn dead_if_quiet(_hist: &VecDeque<(u64, u64)>, _tx: u64) -> bool {
    true
}

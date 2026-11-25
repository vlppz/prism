//! What goes on the wire around plain WireGuard packets.
//!
//! * Before every handshake initiation: `I1..I5` signature packets, then `Jc`
//!   junk datagrams of `Jmin..=Jmax` random bytes, all from the same socket.
//!   A vanilla server drops them silently; DPI sees "DNS + noise" first.
//! * Optional AmneziaWG framing (`S1–S4` padding, `H1–H4` headers) for servers
//!   that run AmneziaWG. With vanilla values this is a no-op.

use std::sync::atomic::{AtomicU32, Ordering};

use rand::Rng;

use crate::config::{Awg, HRange};

pub const INIT_LEN: usize = 148;
pub const RESPONSE_LEN: usize = 92;
pub const COOKIE_LEN: usize = 64;
pub const DATA_MIN_LEN: usize = 32;

/// True for a WireGuard handshake initiation as produced by boringtun.
#[inline]
pub fn is_handshake_init(pkt: &[u8]) -> bool {
    pkt.len() == INIT_LEN && pkt[..4] == [1, 0, 0, 0]
}

pub struct Obfs {
    awg: Awg,
    vanilla: bool,
    counter: AtomicU32,
}

impl Obfs {
    pub fn new(awg: Awg) -> Self {
        let vanilla = awg.is_vanilla_framing();
        Self { awg, vanilla, counter: AtomicU32::new(0) }
    }

    pub fn params(&self) -> &Awg {
        &self.awg
    }

    /// Whether packets go on the wire unchanged (plain WireGuard server).
    #[inline]
    pub fn is_vanilla(&self) -> bool {
        self.vanilla
    }

    /// Packets that must precede a handshake initiation: I1..I5, then junk.
    pub fn preamble(&self) -> Vec<Vec<u8>> {
        let mut rng = rand::rng();
        let counter = self.counter.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let mut out = Vec::with_capacity(self.awg.i.len() + self.awg.jc as usize);
        for p in &self.awg.i {
            let mut buf = Vec::new();
            p.render(&mut buf, counter, &mut rng);
            out.push(buf);
        }
        let (lo, hi) = (self.awg.jmin as usize, (self.awg.jmax as usize).max(self.awg.jmin as usize));
        for _ in 0..self.awg.jc {
            let len = rng.random_range(lo..=hi).max(1);
            let mut buf = vec![0u8; len];
            rng.fill(&mut buf[..]);
            out.push(buf);
        }
        out
    }

    /// Frames an outgoing WireGuard packet into `out`. Returns `false` when
    /// the packet goes on the wire unchanged (`out` untouched).
    pub fn encode_into(&self, pkt: &[u8], out: &mut Vec<u8>) -> bool {
        if self.vanilla || pkt.len() < 4 {
            return false;
        }
        let kind = match pkt[0] {
            1..=4 => (pkt[0] - 1) as usize,
            _ => return false,
        };
        let pad = self.awg.s[kind] as usize;
        let h = pick(self.awg.h[kind]);
        out.clear();
        out.resize(pad, 0);
        rand::rng().fill(&mut out[..pad]);
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&pkt[4..]);
        true
    }

    /// Unframes an incoming datagram in place. Returns the plain WireGuard
    /// packet, or `None` for anything that isn't one (junk from the server).
    pub fn decode<'a>(&self, buf: &'a mut [u8]) -> Option<&'a mut [u8]> {
        if self.vanilla {
            return Some(buf);
        }
        let n = buf.len();
        let s = &self.awg.s;
        let h = &self.awg.h;
        let hdr_at = |off: usize| -> Option<u32> {
            buf.get(off..off + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let candidates = [
            (0usize, INIT_LEN, true),
            (1, RESPONSE_LEN, true),
            (2, COOKIE_LEN, true),
            (3, DATA_MIN_LEN, false),
        ];
        for (kind, len, exact) in candidates {
            let off = s[kind] as usize;
            let ok_len = if exact { n == off + len } else { n >= off + len };
            if ok_len && hdr_at(off).is_some_and(|v| h[kind].contains(v)) {
                let out = &mut buf[off..];
                out[..4].copy_from_slice(&(kind as u32 + 1).to_le_bytes());
                return Some(out);
            }
        }
        None
    }
}

fn pick(r: HRange) -> u32 {
    if r.lo == r.hi {
        r.lo
    } else {
        rand::rng().random_range(r.lo..=r.hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Awg;

    fn init_packet() -> Vec<u8> {
        let mut p = vec![0u8; INIT_LEN];
        p[0] = 1;
        p[4..].iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
        p
    }

    #[test]
    fn detects_handshake_init() {
        assert!(is_handshake_init(&init_packet()));
        let mut data = vec![0u8; 148];
        data[0] = 4;
        assert!(!is_handshake_init(&data));
    }

    #[test]
    fn vanilla_is_identity() {
        let o = Obfs::new(Awg::bypass_default());
        let p = init_packet();
        assert!(!o.encode_into(&p, &mut Vec::new()));
        let mut buf = p.clone();
        assert_eq!(o.decode(&mut buf).unwrap(), &p[..]);
    }

    #[test]
    fn preamble_sizes() {
        let o = Obfs::new(Awg::bypass_default());
        let sizes: Vec<usize> = o.preamble().iter().map(Vec::len).collect();
        assert_eq!(sizes.len(), 1 + 8);
        assert_eq!(sizes[0], 29); // DNS query
        assert!(sizes[1..].iter().all(|s| (100..=900).contains(s)));
    }

    #[test]
    fn awg_framing_round_trip() {
        let mut awg = Awg::bypass_default();
        awg.s = [15, 30, 0, 8];
        awg.h = [HRange::one(1111), HRange::one(2222), HRange::one(3333), HRange { lo: 5000, hi: 6000 }];
        let o = Obfs::new(awg);
        for kind in 1u8..=4 {
            let len = match kind {
                1 => INIT_LEN,
                2 => RESPONSE_LEN,
                3 => COOKIE_LEN,
                _ => 200,
            };
            let mut p = vec![0u8; len];
            p[0] = kind;
            p[4..].iter_mut().enumerate().for_each(|(i, b)| *b = (i * 7) as u8);
            let mut wire = Vec::new();
            assert!(o.encode_into(&p, &mut wire));
            assert_ne!(wire, p);
            let back = o.decode(&mut wire).expect("decodes");
            assert_eq!(back, &p[..], "kind {kind}");
        }
        // junk from the server is dropped
        let mut junk = vec![9u8; 77];
        assert!(o.decode(&mut junk).is_none());
    }
}

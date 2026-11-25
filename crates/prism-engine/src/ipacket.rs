//! AmneziaWG "signature packet" templates (`I1`–`I5`).
//!
//! A template is a sequence of tags:
//! * `<b 0xHEX>` fixed bytes
//! * `<r N>`     N random bytes
//! * `<rc N>`    N random ASCII letters
//! * `<rd N>`    N random digits
//! * `<c>`       32-bit packet counter, big-endian
//! * `<t>`       32-bit unix time, big-endian

use anyhow::{bail, Context, Result};
use rand::Rng;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tag {
    Bytes(Vec<u8>),
    Random(usize),
    RandomChars(usize),
    RandomDigits(usize),
    Counter,
    Time,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IPacket(Vec<Tag>);

const MAX_LEN: usize = 1400;

impl IPacket {
    pub fn parse(s: &str) -> Result<Self> {
        let mut tags = Vec::new();
        let mut rest = s.trim();
        let mut total = 0usize;
        while !rest.is_empty() {
            let Some(body) = rest.strip_prefix('<') else { bail!("expected '<' in {s:?}") };
            let end = body.find('>').context("unclosed '<'")?;
            let (inner, after) = (&body[..end], &body[end + 1..]);
            rest = after.trim_start();
            let mut parts = inner.split_whitespace();
            let kind = parts.next().unwrap_or("");
            let arg = parts.next();
            let n = || -> Result<usize> {
                arg.context("missing length")?.parse::<usize>().context("bad length")
            };
            let tag = match kind {
                "b" => {
                    let hex = arg.context("missing hex")?;
                    let hex = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X")).unwrap_or(hex);
                    if hex.len() % 2 != 0 {
                        bail!("odd hex length");
                    }
                    let bytes = (0..hex.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                        .collect::<Result<Vec<_>, _>>()
                        .context("bad hex")?;
                    total += bytes.len();
                    Tag::Bytes(bytes)
                }
                "r" => {
                    let n = n()?;
                    total += n;
                    Tag::Random(n)
                }
                "rc" => {
                    let n = n()?;
                    total += n;
                    Tag::RandomChars(n)
                }
                "rd" => {
                    let n = n()?;
                    total += n;
                    Tag::RandomDigits(n)
                }
                "c" => {
                    total += 4;
                    Tag::Counter
                }
                "t" => {
                    total += 4;
                    Tag::Time
                }
                other => bail!("unknown tag <{other}>"),
            };
            tags.push(tag);
        }
        if tags.is_empty() {
            bail!("empty packet");
        }
        if total > MAX_LEN {
            bail!("packet longer than {MAX_LEN} bytes");
        }
        Ok(Self(tags))
    }

    /// Renders the packet into `out` (cleared first).
    pub fn render(&self, out: &mut Vec<u8>, counter: u32, rng: &mut impl Rng) {
        out.clear();
        for t in &self.0 {
            match t {
                Tag::Bytes(b) => out.extend_from_slice(b),
                Tag::Random(n) => {
                    let start = out.len();
                    out.resize(start + n, 0);
                    rng.fill(&mut out[start..]);
                }
                Tag::RandomChars(n) => {
                    const A: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
                    out.extend((0..*n).map(|_| A[rng.random_range(0..A.len())]));
                }
                Tag::RandomDigits(n) => out.extend((0..*n).map(|_| b'0' + rng.random_range(0..10u8))),
                Tag::Counter => out.extend_from_slice(&counter.to_be_bytes()),
                Tag::Time => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as u32)
                        .unwrap_or(0);
                    out.extend_from_slice(&now.to_be_bytes());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DNS_I1;

    #[test]
    fn dns_i1_matches_the_working_recipe() {
        let p = IPacket::parse(DNS_I1).unwrap();
        let mut out = Vec::new();
        p.render(&mut out, 0, &mut rand::rng());
        // 2 random bytes (DNS transaction id), then the fixed query for example.com A IN.
        let fixed = [
            0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, b'e', b'x', b'a', b'm', b'p', b'l',
            b'e', 0x03, b'c', b'o', b'm', 0x00, 0x00, 0x01, 0x00, 0x01,
        ];
        assert_eq!(out.len(), 2 + fixed.len());
        assert_eq!(&out[2..], &fixed);
    }

    #[test]
    fn all_tags() {
        let p = IPacket::parse("<b 0xff><r 3><rc 4><rd 2><c><t>").unwrap();
        let mut out = Vec::new();
        p.render(&mut out, 7, &mut rand::rng());
        assert_eq!(out.len(), 1 + 3 + 4 + 2 + 4 + 4);
        assert_eq!(out[0], 0xff);
        assert!(out[4..8].iter().all(u8::is_ascii_alphabetic));
        assert!(out[8..10].iter().all(u8::is_ascii_digit));
        assert_eq!(&out[10..14], &7u32.to_be_bytes());
    }

    #[test]
    fn rejects_garbage() {
        assert!(IPacket::parse("<x 1>").is_err());
        assert!(IPacket::parse("<b 0xf>").is_err());
        assert!(IPacket::parse("<r 5000>").is_err());
        assert!(IPacket::parse("hello").is_err());
    }
}

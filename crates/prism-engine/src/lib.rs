//! Prism engine: a userspace WireGuard client that slips past DPI (TSPU) by
//! sending junk and DNS-shaped packets before every handshake, like
//! AmneziaWG does, while talking plain WireGuard to the server.

pub mod config;
pub mod dns;
pub mod geoip;
pub mod ipacket;
pub mod net;
pub mod obfs;
pub mod proto;
pub mod tunnel;

pub use config::{parse, suggest_name, WgConfig};
pub use proto::{Event, Phase};
pub use tunnel::{run, Options};

//! Messages between the app and the macOS root helper, and engine events.
//!
//! The helper is a launchd daemon listening on a root-owned socket. The app
//! connects, receives [`HelperMsg::Daemon`], sends [`AppMsg::Start`] with the
//! `.conf` text, then gets [`HelperMsg::Event`]s. Closing the connection
//! tears the tunnel down, so a crashed app never leaves routes behind.

use serde::{Deserialize, Serialize};

pub const DAEMON_SOCKET: &str = "/var/run/app.prism.vpn.helper.sock";
pub const DAEMON_DIR: &str = "/Library/PrivilegedHelperTools/app.prism.vpn";
pub const DAEMON_LABEL: &str = "app.prism.vpn.helper";
pub const DAEMON_PLIST: &str = "/Library/LaunchDaemons/app.prism.vpn.helper.plist";
pub const STATE_DIR: &str = "/Library/Application Support/app.prism.vpn.helper";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Setting up / waiting for the first handshake.
    Connecting,
    /// Handshake done and traffic flows.
    Connected,
    /// No handshake reply for a while (IP probably blocked).
    Unreachable,
    /// Handshake ok but nothing comes back (DPI); reconnecting harder.
    Stalled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    Phase { phase: Phase },
    Stats { rx: u64, tx: u64, handshake_age: Option<u64>, level: u8 },
    Log { line: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HelperMsg {
    Daemon { version: String },
    Event { event: Event },
    Exited { error: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AppMsg {
    Start {
        config: String,
        no_junk: bool,
        /// Smart routing: Russian networks bypass the tunnel.
        #[serde(default)]
        smart: bool,
    },
    Stop,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let m = HelperMsg::Event { event: Event::Phase { phase: Phase::Connected } };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"type":"event","event":{"type":"phase","phase":"connected"}}"#
        );
        assert_eq!(serde_json::from_str::<AppMsg>(r#"{"type":"stop"}"#).unwrap(), AppMsg::Stop);
    }
}

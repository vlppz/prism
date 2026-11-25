//! Points the OS resolver at the config's `DNS =` servers while connected.
//!
//! macOS: every enabled network service gets the servers (old values are
//! backed up to a file so a crash can be repaired on next start).
//! Windows: the servers are set on the TUN adapter itself (see `tunnel.rs`),
//! which disappears on disconnect, so nothing needs restoring.

use std::net::IpAddr;
use std::path::Path;

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn backup_file(state: &Path) -> PathBuf {
        state.join("dns-backup.json")
    }

    fn networksetup(args: &[&str]) -> Option<String> {
        let out = Command::new("/usr/sbin/networksetup").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn services() -> Vec<String> {
        networksetup(&["-listallnetworkservices"])
            .unwrap_or_default()
            .lines()
            .skip(1) // "An asterisk (*) denotes that a network service is disabled."
            .filter(|l| !l.starts_with('*') && !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    fn current_dns(service: &str) -> Vec<String> {
        networksetup(&["-getdnsservers", service])
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| l.parse::<IpAddr>().is_ok())
            .map(str::to_string)
            .collect()
    }

    fn set_dns(service: &str, servers: &[String]) {
        let mut args = vec!["-setdnsservers", service];
        if servers.is_empty() {
            args.push("Empty");
        } else {
            args.extend(servers.iter().map(String::as_str));
        }
        networksetup(&args);
    }

    fn flush() {
        let _ = Command::new("/usr/bin/dscacheutil").arg("-flushcache").status();
        let _ = Command::new("/usr/bin/killall").args(["-HUP", "mDNSResponder"]).status();
    }

    pub fn apply(state: &Path, servers: &[IpAddr]) {
        let _ = std::fs::create_dir_all(state);
        let file = backup_file(state);
        if !file.exists() {
            let backup: BTreeMap<String, Vec<String>> = services()
                .into_iter()
                .map(|s| {
                    let d = current_dns(&s);
                    (s, d)
                })
                .collect();
            if let Ok(json) = serde_json::to_vec(&backup) {
                let _ = std::fs::write(&file, json);
            }
        }
        let servers: Vec<String> = servers.iter().map(IpAddr::to_string).collect();
        for service in services() {
            set_dns(&service, &servers);
        }
        flush();
    }

    pub fn restore(state: &Path) {
        let file = backup_file(state);
        let Ok(data) = std::fs::read(&file) else { return };
        if let Ok(backup) = serde_json::from_slice::<BTreeMap<String, Vec<String>>>(&data) {
            for (service, servers) in backup {
                set_dns(&service, &servers);
            }
            flush();
        }
        let _ = std::fs::remove_file(file);
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::net::IpAddr;
    use std::path::Path;

    pub fn apply(_state: &Path, _servers: &[IpAddr]) {}
    pub fn restore(_state: &Path) {}
}

pub fn apply(state: &Path, servers: &[IpAddr]) {
    if !servers.is_empty() {
        imp::apply(state, servers)
    }
}

/// Restores the saved DNS settings. Also called at startup, in case a
/// previous session died before restoring.
pub fn restore(state: &Path) {
    imp::restore(state)
}

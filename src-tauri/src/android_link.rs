//! Android: the window's end of the session socket (the VPN process serves
//! it, see android_core.rs). The window mirrors the session's state from it
//! and sends `stop` and `fg 0|1` (on screen or not) through it.
//!
//! The VPN process only exists while Prism's VPN service does, so the window
//! connects when it opens, when it comes back on screen and right after it
//! asked the service to connect; a closed socket means the VPN is gone.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use tauri::{AppHandle, Emitter};

use crate::{set_status, Stats, Status};

struct Link {
    app: AppHandle,
    path: PathBuf,
    /// The open connection's writing half.
    conn: Mutex<Option<UnixStream>>,
    /// Keep trying to connect until then (the service is starting).
    until: Mutex<Option<Instant>>,
    wake: (std::sync::Mutex<bool>, Condvar),
    fg: AtomicBool,
}

static LINK: OnceLock<Link> = OnceLock::new();

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    t: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    config_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    since: Option<u64>,
    #[serde(default)]
    rx: u64,
    #[serde(default)]
    tx: u64,
    #[serde(default)]
    handshake_age: Option<u64>,
    #[serde(default)]
    level: u8,
}

fn state_str(s: &str) -> &'static str {
    match s {
        "connecting" => "connecting",
        "connected" => "connected",
        "unreachable" => "unreachable",
        "stalled" => "stalled",
        _ => "off",
    }
}

pub fn start(app: AppHandle, data_dir: PathBuf) {
    let link = Link {
        app,
        path: data_dir.join(crate::android_core::SOCKET),
        conn: Mutex::new(None),
        until: Mutex::new(None),
        wake: (std::sync::Mutex::new(true), Condvar::new()),
        fg: AtomicBool::new(true),
    };
    if LINK.set(link).is_ok() {
        std::thread::Builder::new().name("prism-link".into()).spawn(run).ok();
    }
}

/// Try to connect now (opened, back on screen).
pub fn kick() {
    if let Some(l) = LINK.get() {
        *l.wake.0.lock().unwrap() = true;
        l.wake.1.notify_one();
    }
}

/// The service was asked to connect: keep trying until it's up.
pub fn expect_service() {
    if let Some(l) = LINK.get() {
        if l.conn.lock().is_none() {
            *l.until.lock() = Some(Instant::now() + Duration::from_secs(20));
            kick();
        }
    }
}

pub fn connected() -> bool {
    LINK.get().is_some_and(|l| l.conn.lock().is_some())
}

fn send(line: &str) -> bool {
    let Some(l) = LINK.get() else { return false };
    let mut conn = l.conn.lock();
    match conn.as_mut() {
        Some(s) => s.write_all(format!("{line}\n").as_bytes()).is_ok(),
        None => false,
    }
}

/// Disconnect: the VPN process stops the tunnel and reports `off`.
pub fn stop() -> bool {
    send("stop")
}

pub fn set_foreground(on: bool) {
    if let Some(l) = LINK.get() {
        l.fg.store(on, Ordering::Relaxed);
        send(if on { "fg 1" } else { "fg 0" });
        if on && !connected() {
            kick();
        }
    }
}

fn run() {
    let l = LINK.get().expect("link");
    loop {
        // wait for a reason to (re)connect
        {
            let mut woken = l.wake.0.lock().unwrap();
            while !*woken {
                let retrying = l.until.lock().is_some_and(|t| Instant::now() < t);
                let wait = if retrying { Duration::from_millis(400) } else { Duration::from_secs(3600) };
                woken = l.wake.1.wait_timeout(woken, wait).unwrap().0;
                if retrying {
                    break;
                }
            }
            *woken = false;
        }
        let Ok(stream) = UnixStream::connect(&l.path) else {
            let expired = l.until.lock().is_some_and(|t| Instant::now() >= t);
            if expired {
                *l.until.lock() = None;
                // asked to connect, but the service never came up
                let app = &l.app;
                set_status(app, |s| {
                    if s.state == "connecting" {
                        *s = Status { state: "off", config_id: s.config_id.take(), error: Some("Prism's VPN service didn't start. Try again".into()), since: None };
                    }
                });
            }
            continue;
        };
        *l.until.lock() = None;
        let Ok(writer) = stream.try_clone() else { continue };
        *l.conn.lock() = Some(writer);
        send(if l.fg.load(Ordering::Relaxed) { "fg 1" } else { "fg 0" });
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let Ok(m) = serde_json::from_str::<Line>(&line) else { continue };
            match m.t.as_str() {
                "status" => {
                    let state = state_str(m.state.as_deref().unwrap_or("off"));
                    set_status(&l.app, |s| *s = Status { state, config_id: m.config_id, error: m.error, since: m.since });
                }
                "stats" => {
                    let _ = l.app.emit("stats", Stats { rx: m.rx, tx: m.tx, handshake_age: m.handshake_age, level: m.level });
                }
                _ => {}
            }
        }
        // the VPN process went away, and the tunnel with it
        *l.conn.lock() = None;
        set_status(&l.app, |s| {
            if s.state != "off" {
                *s = Status { state: "off", config_id: s.config_id.take(), error: None, since: None };
            }
        });
    }
}

// Called from VpnPlugin.kt (the window's process).

#[no_mangle]
pub extern "system" fn Java_app_prism_vpn_PrismNative_setForeground(_env: *mut std::ffi::c_void, _class: *mut std::ffi::c_void, on: u8) {
    set_foreground(on != 0);
}

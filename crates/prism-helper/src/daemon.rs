//! macOS launchd daemon, so connecting doesn't ask for a password each time.
//!
//! `install` (run once, elevated) copies this helper into a root-owned
//! directory and registers the daemon. The daemon listens on a socket in
//! `/var/run` (only root can create it there, so it can't be spoofed), serves
//! only the uid that installed it, and runs one session per connection: the
//! session ends when the app disconnects, quits or crashes.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use prism_engine::proto::{AppMsg, HelperMsg, DAEMON_DIR, DAEMON_LABEL, DAEMON_PLIST, DAEMON_SOCKET, STATE_DIR};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const NAME: &str = "prism-helper";

pub fn uid_arg(mut args: impl Iterator<Item = String>) -> Result<u32> {
    match (args.next().as_deref(), args.next()) {
        (Some("--uid"), Some(uid)) => uid.parse().context("--uid must be a number"),
        _ => bail!("expected --uid <uid>"),
    }
}

fn require_root() -> Result<()> {
    // SAFETY: plain syscall.
    if unsafe { libc::geteuid() } != 0 {
        bail!("must run as root");
    }
    Ok(())
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, msg: &HelperMsg) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg).expect("serializable");
    line.push(b'\n');
    w.write_all(&line).await
}

pub async fn serve(uid: u32) -> Result<()> {
    require_root()?;
    std::fs::create_dir_all(STATE_DIR)?;
    // A previous instance may have been killed mid-session.
    prism_engine::dns::restore(Path::new(STATE_DIR));

    let _ = std::fs::remove_file(DAEMON_SOCKET);
    let listener = UnixListener::bind(DAEMON_SOCKET).context("binding daemon socket")?;
    // Anyone may connect; the peer uid is checked per connection.
    std::fs::set_permissions(DAEMON_SOCKET, std::fs::Permissions::from_mode(0o666))?;
    eprintln!("{NAME} {VERSION} serving uid {uid}");

    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                eprintln!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        match stream.peer_cred().map(|c| c.uid()) {
            Ok(peer) if peer == uid || peer == 0 => {}
            Ok(peer) => {
                eprintln!("rejected connection from uid {peer}");
                continue;
            }
            Err(_) => continue, // probe that already hung up
        }
        // One session at a time.
        if let Err(e) = handle(stream).await {
            eprintln!("session: {e:#}");
        }
    }
}

async fn handle(stream: UnixStream) -> Result<()> {
    let (rx, mut tx) = stream.into_split();
    send(&mut tx, &HelperMsg::Daemon { version: VERSION.into() }).await?;
    let mut lines = BufReader::new(rx).lines();
    let first = match tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await {
        Ok(Ok(Some(line))) => line,
        _ => return Ok(()), // probe or idle client
    };
    let Ok(AppMsg::Start { config, no_junk, smart }) = serde_json::from_str::<AppMsg>(&first) else {
        return Ok(());
    };
    let cfg = match prism_engine::parse(&config) {
        Ok(c) => c,
        Err(e) => {
            send(&mut tx, &HelperMsg::Exited { error: Some(format!("{e:#}")) }).await?;
            return Ok(());
        }
    };

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = watch::channel(false);
    let opts = prism_engine::Options { no_junk, state_dir: STATE_DIR.into(), wintun_path: None, smart };
    let mut session = tokio::spawn(prism_engine::run(cfg, opts, ev_tx, stop_rx));

    // Forward events; stop on `Stop` or when the app goes away.
    let result = loop {
        tokio::select! {
            r = &mut session => break r.unwrap_or_else(|e| Err(anyhow::anyhow!("Prism's network service crashed ({e})"))),
            Some(e) = ev_rx.recv() => {
                if send(&mut tx, &HelperMsg::Event { event: e }).await.is_err() {
                    let _ = stop_tx.send(true);
                }
            }
            line = lines.next_line() => match line {
                Ok(Some(l)) if matches!(serde_json::from_str::<AppMsg>(&l), Ok(AppMsg::Stop)) => { let _ = stop_tx.send(true); }
                Ok(Some(_)) => {}
                _ => { let _ = stop_tx.send(true); } // app closed the socket
            },
        }
    };
    while let Ok(e) = ev_rx.try_recv() {
        let _ = send(&mut tx, &HelperMsg::Event { event: e }).await;
    }
    // the app shows the top-level sentence; the details go to the log
    let error = result.err().map(|e| {
        eprintln!("session ended: {e:#}");
        e.to_string()
    });
    let _ = send(&mut tx, &HelperMsg::Exited { error }).await;
    Ok(())
}

/// `std::fs::copy` may keep the source's owner, so ownership is set
/// explicitly: anything root executes must not be writable by the user.
fn make_root_owned(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: valid NUL-terminated path; uid/gid 0 = root:wheel.
    if unsafe { libc::chown(c.as_ptr(), 0, 0) } != 0 {
        bail!("chown {}: {}", path.display(), std::io::Error::last_os_error());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn launchctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("/bin/launchctl").args(args).output()?;
    if !out.status.success() {
        bail!("launchctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn plist(uid: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{DAEMON_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{DAEMON_DIR}/{NAME}</string>
    <string>daemon</string>
    <string>--uid</string>
    <string>{uid}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/Library/Logs/prism-helper.log</string>
</dict>
</plist>
"#
    )
}

/// Copies the helper into a root-owned directory and (re)starts the daemon.
pub fn install(uid: u32) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("the background service is only used on macOS");
    }
    require_root()?;
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = Path::new(DAEMON_DIR);
    std::fs::create_dir_all(dir)?;
    make_root_owned(dir, 0o755)?;
    let tmp = dir.join(format!("{NAME}.new"));
    // copy-then-rename: a running daemon keeps its old inode
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(&exe, &tmp).with_context(|| format!("copying {}", exe.display()))?;
    make_root_owned(&tmp, 0o755)?;
    std::fs::rename(&tmp, dir.join(NAME))?;
    std::fs::write(DAEMON_PLIST, plist(uid))?;
    make_root_owned(Path::new(DAEMON_PLIST), 0o644)?;

    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
    // bootout is asynchronous; retry bootstrap briefly
    let mut last = Ok(());
    for _ in 0..20 {
        last = launchctl(&["bootstrap", "system", DAEMON_PLIST]);
        if last.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    last
}

pub fn uninstall() -> Result<()> {
    require_root()?;
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
    prism_engine::dns::restore(Path::new(STATE_DIR));
    let _ = std::fs::remove_file(DAEMON_PLIST);
    let _ = std::fs::remove_file(DAEMON_SOCKET);
    let _ = std::fs::remove_dir_all(DAEMON_DIR);
    let _ = std::fs::remove_dir_all(STATE_DIR);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_root_owned_copy() {
        let p = plist(501);
        assert!(p.contains("<string>/Library/PrivilegedHelperTools/app.prism.vpn/prism-helper</string>"));
        assert!(p.contains("<string>--uid</string>\n    <string>501</string>"));
    }
}

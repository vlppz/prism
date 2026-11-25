//! Runs the tunnel: through the root launchd daemon on macOS, in-process on
//! Windows (the app itself runs elevated there).

use std::time::Duration;

use anyhow::Result;
use prism_engine::Event;
use tokio::sync::{mpsc, oneshot};

/// Everything the UI needs to know about the running session.
pub enum Update {
    Event(Event),
    /// Session ended; `Some` = it failed.
    Ended(Option<String>),
}

pub struct Session {
    stop: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<()>>,
}

impl Session {
    /// Starts a session. `updates` receives events until `Ended`.
    pub async fn start(config: String, smart: bool, updates: mpsc::UnboundedSender<Update>) -> Result<Self> {
        let no_junk = std::env::var_os("PRISM_NO_JUNK").is_some();
        imp::start(config, no_junk, smart, updates).await
    }

    pub async fn stop(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(d) = self.done.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), d).await;
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use anyhow::{anyhow, bail, Context, Result};
    use prism_engine::proto::{AppMsg, HelperMsg, DAEMON_DIR, DAEMON_SOCKET};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::process::Command;
    use tokio::sync::{mpsc, oneshot};

    use super::{Session, Update};

    const AUTH_TIMEOUT: Duration = Duration::from_secs(180);

    fn bundled_helper() -> Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let p = exe.parent().context("app has no directory")?.join("prism-helper");
        if !p.is_file() {
            bail!("Prism's network service is missing from the app ({}). Reinstall Prism", p.display());
        }
        Ok(p)
    }

    /// The installed daemon is current if it is byte-identical to the bundled
    /// helper, root-owned and not writable by anyone else.
    fn installed_is_current(bundled: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let installed = Path::new(DAEMON_DIR).join("prism-helper");
        match (std::fs::metadata(bundled), std::fs::metadata(&installed)) {
            (Ok(a), Ok(b)) if a.len() == b.len() && b.uid() == 0 && b.mode() & 0o022 == 0 => {
                matches!((std::fs::read(bundled), std::fs::read(&installed)), (Ok(x), Ok(y)) if x == y)
            }
            _ => false,
        }
    }

    async fn reachable() -> bool {
        UnixStream::connect(DAEMON_SOCKET).await.is_ok()
    }

    fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    fn describe_auth_failure(code: Option<i32>, stderr: &str) -> String {
        let stderr = stderr.trim();
        if stderr.contains("-128") || stderr.contains("User canceled") {
            "Prism needs your administrator password once to set up its network service".into()
        } else if stderr.is_empty() {
            format!("couldn't get administrator rights (exit code {code:?})")
        } else {
            format!("couldn't get administrator rights: {stderr}")
        }
    }

    /// Installs or updates the daemon: the only time a password is asked.
    async fn ensure_installed() -> Result<()> {
        let helper = bundled_helper()?;
        let h = helper.clone();
        let current = tokio::task::spawn_blocking(move || installed_is_current(&h)).await?;
        if current && reachable().await {
            return Ok(());
        }
        // SAFETY: plain syscall.
        let uid = unsafe { libc::getuid() };
        let shell = format!("{} install --uid {uid}", sh_quote(&helper.to_string_lossy()));
        let script = format!(
            "do shell script \"{}\" with prompt \"Prism needs to install its network service once. After that it connects without asking for your password.\" with administrator privileges",
            shell.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let out = tokio::time::timeout(AUTH_TIMEOUT, Command::new("/usr/bin/osascript").arg("-e").arg(script).output())
            .await
            .map_err(|_| anyhow!("timed out waiting for administrator approval"))??;
        if !out.status.success() {
            bail!("{}", describe_auth_failure(out.status.code(), &String::from_utf8_lossy(&out.stderr)));
        }
        for _ in 0..50 {
            if reachable().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("Prism's network service isn't responding. Details are in /Library/Logs/prism-helper.log")
    }

    pub async fn start(config: String, no_junk: bool, smart: bool, updates: mpsc::UnboundedSender<Update>) -> Result<Session> {
        ensure_installed().await?;
        let stream = UnixStream::connect(DAEMON_SOCKET).await.context("couldn't reach Prism's network service")?;
        let (rx, mut tx) = stream.into_split();
        let mut lines = BufReader::new(rx).lines();
        let hello = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .map_err(|_| anyhow!("Prism's network service is busy with another connection"))??
            .context("Prism's network service closed the connection")?;
        if !matches!(serde_json::from_str::<HelperMsg>(&hello), Ok(HelperMsg::Daemon { .. })) {
            bail!("Prism's network service answered unexpectedly. Reinstall Prism");
        }
        let mut msg = serde_json::to_vec(&AppMsg::Start { config, no_junk, smart })?;
        msg.push(b'\n');
        tx.write_all(&msg).await?;

        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let (done_tx, done_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut stopping = false;
            let mut error = None;
            loop {
                tokio::select! {
                    _ = &mut stop_rx, if !stopping => {
                        stopping = true;
                        let mut m = serde_json::to_vec(&AppMsg::Stop).unwrap();
                        m.push(b'\n');
                        if tx.write_all(&m).await.is_err() { break; }
                    }
                    line = lines.next_line() => match line {
                        Ok(Some(l)) => match serde_json::from_str::<HelperMsg>(&l) {
                            Ok(HelperMsg::Event { event }) => { let _ = updates.send(Update::Event(event)); }
                            Ok(HelperMsg::Exited { error: e }) => { error = e; break; }
                            _ => {}
                        },
                        _ => {
                            if !stopping { error = Some("Prism's network service stopped unexpectedly".into()); }
                            break;
                        }
                    }
                }
            }
            let _ = updates.send(Update::Ended(error));
            let _ = done_tx.send(());
        });
        Ok(Session { stop: Some(stop_tx), done: Some(done_rx) })
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::Result;
    use tokio::sync::{mpsc, oneshot, watch};

    use super::{Session, Update};

    pub async fn start(config: String, no_junk: bool, smart: bool, updates: mpsc::UnboundedSender<Update>) -> Result<Session> {
        let cfg = prism_engine::parse(&config)?;
        let dir = std::env::current_exe()?.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let opts = prism_engine::Options {
            no_junk,
            state_dir: std::env::temp_dir().join("prism"),
            wintun_path: Some(dir.join("wintun.dll").to_string_lossy().into_owned()),
            smart,
        };
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (stop_watch, stop_rx) = watch::channel(false);
        let (stop_tx, stop_signal) = oneshot::channel::<()>();
        let (done_tx, done_rx) = oneshot::channel();
        let mut session = tokio::spawn(prism_engine::run(cfg, opts, ev_tx, stop_rx));
        tokio::spawn(async move {
            let _ = stop_signal.await;
            let _ = stop_watch.send(true);
            // keep the sender alive until the session is gone
            stop_watch.closed().await;
        });
        tokio::spawn(async move {
            let result = loop {
                tokio::select! {
                    r = &mut session => break r.unwrap_or_else(|e| Err(anyhow::anyhow!("the VPN session crashed ({e})"))),
                    Some(e) = ev_rx.recv() => { let _ = updates.send(Update::Event(e)); }
                }
            };
            while let Ok(e) = ev_rx.try_recv() {
                let _ = updates.send(Update::Event(e));
            }
            let _ = updates.send(Update::Ended(result.err().map(|e| {
                eprintln!("session ended: {e:#}");
                e.to_string()
            })));
            let _ = done_tx.send(());
        });
        Ok(Session { stop: Some(stop_tx), done: Some(done_rx) })
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use anyhow::{bail, Result};
    use tokio::sync::mpsc;

    use super::{Session, Update};

    pub async fn start(_config: String, _no_junk: bool, _smart: bool, _updates: mpsc::UnboundedSender<Update>) -> Result<Session> {
        bail!("Prism supports macOS and Windows")
    }
}

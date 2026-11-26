//! The Android session. It lives in the VPN service's own process (":vpn"),
//! apart from the window's: swiping Prism away ends the window's process
//! (Tauri exits with its last window) and the tunnel doesn't notice.
//!
//! The service (PrismVpnService.kt) drives it: it asks for a [`plan`], brings
//! the interface up and hands the fd to [`run`]. So the tunnel starts the same
//! way from the app, the Quick Settings tile or a restart after Android killed
//! the process. The window, when open, follows it over a Unix socket in the
//! app's data directory ([`serve`]; the other end is android_link.rs): one
//! JSON line per change, and it can send `stop` and `fg 0|1`.
//!
//! Everything Kotlin calls is a `PrismNative` JNI function at the bottom.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, LazyLock, Once};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use prism_engine::{Event, Phase, WgConfig};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};

use crate::store::Store;

/// What the app and the notification show.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snap {
    /// off | connecting | connected | unreachable | stalled
    pub state: &'static str,
    pub config_id: Option<String>,
    pub name: Option<String>,
    pub error: Option<String>,
    /// Unix ms when the tunnel first came up.
    pub since: Option<u64>,
    pub rx: u64,
    pub tx: u64,
}

impl Default for Snap {
    fn default() -> Self {
        Self { state: "off", config_id: None, name: None, error: None, since: None, rx: 0, tx: 0 }
    }
}

/// The socket the window connects to, in the app's data directory.
pub const SOCKET: &str = "core.sock";

/// A connected window.
struct Client {
    /// Lines to write; a window that doesn't read (frozen in the background)
    /// loses lines instead of stalling the tunnel.
    tx: SyncSender<String>,
    /// On screen: wants the per-second traffic figures.
    fg: Arc<AtomicBool>,
}

struct Running {
    stop: watch::Sender<bool>,
    done: oneshot::Receiver<()>,
}

struct Pending {
    cfg: WgConfig,
}

struct Core {
    rt: tokio::runtime::Runtime,
    snap: Mutex<Snap>,
    pending: Mutex<Option<Pending>>,
    running: Mutex<Option<Running>>,
    clients: Mutex<Vec<Client>>,
    /// Bumped by every start and stop, so a finished session's last words
    /// can't overwrite a newer one's state.
    generation: AtomicU64,
}

static CORE: LazyLock<Core> = LazyLock::new(|| Core {
    rt: tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("prism-tunnel")
        .enable_all()
        .build()
        .expect("tokio runtime"),
    snap: Mutex::new(Snap::default()),
    pending: Mutex::new(None),
    running: Mutex::new(None),
    clients: Mutex::new(Vec::new()),
    generation: AtomicU64::new(0),
});

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn status_line(s: &Snap) -> String {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    v["t"] = "status".into();
    v.to_string()
}

/// Sends a line to the connected windows (`stats_only`: just those on screen),
/// dropping any that went away.
fn broadcast(line: String, stats_only: bool) {
    CORE.clients.lock().retain(|c| {
        if stats_only && !c.fg.load(Ordering::Relaxed) {
            return true;
        }
        !matches!(c.tx.try_send(line.clone()), Err(std::sync::mpsc::TrySendError::Disconnected(_)))
    });
}

fn update(f: impl FnOnce(&mut Snap)) {
    let snap = {
        let mut s = CORE.snap.lock();
        f(&mut s);
        s.clone()
    };
    broadcast(status_line(&snap), false);
}

/// Listens for the window (once per process).
pub fn serve(data_dir: &str) {
    static STARTED: Once = Once::new();
    let path = Path::new(data_dir).join(SOCKET);
    STARTED.call_once(move || {
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => return eprintln!("core socket {}: {e}", path.display()),
        };
        std::thread::Builder::new()
            .name("prism-core-sock".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    accept(stream);
                }
            })
            .ok();
    });
}

fn accept(stream: UnixStream) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let (tx, rx) = sync_channel::<String>(64);
    let fg = Arc::new(AtomicBool::new(true));
    // the current state first, then every change
    let _ = tx.try_send(status_line(&snapshot()));
    CORE.clients.lock().push(Client { tx, fg: fg.clone() });
    std::thread::spawn(move || {
        for mut line in rx {
            line.push('\n');
            if writer.write_all(line.as_bytes()).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            match line.trim() {
                "stop" => stop_blocking(),
                "fg 1" => fg.store(true, Ordering::Relaxed),
                "fg 0" => fg.store(false, Ordering::Relaxed),
                _ => {}
            }
        }
    });
}

pub fn snapshot() -> Snap {
    CORE.snap.lock().clone()
}

pub fn is_running() -> bool {
    CORE.snap.lock().state != "off"
}

/// Whether there is a server to connect to.
pub fn has_config(data_dir: &str) -> bool {
    Store::open(PathBuf::from(data_dir)).map(|s| s.index().selected.is_some()).unwrap_or(false)
}

/// Step 1 (service thread): reads the selected config and returns what
/// VpnService.Builder needs, as JSON.
pub fn plan(data_dir: &str) -> Result<String> {
    stop_blocking();
    let store = Store::open(PathBuf::from(data_dir))?;
    let index = store.index();
    let Some(id) = index.selected.clone() else { bail!("import a WireGuard config first") };
    let name = index.items.iter().find(|i| i.id == id).map(|i| i.name.clone());
    let cfg = prism_engine::parse(&store.read(&id)?).context("the saved config can't be read")?;
    let plan = crate::plan::plan(&cfg, index.smart);
    *CORE.pending.lock() = Some(Pending { cfg });
    CORE.generation.fetch_add(1, Ordering::SeqCst);
    update(|s| *s = Snap { state: "connecting", config_id: Some(id), name, ..Default::default() });
    Ok(serde_json::to_string(&plan)?)
}

/// Step 2: the interface is up; run the engine on it.
pub fn run(fd: i32) {
    // SAFETY: the service detached this fd for us; nothing else closes it.
    let tun_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let Some(Pending { cfg }) = CORE.pending.lock().take() else {
        drop(tun_fd);
        return failed("the session was cancelled".into());
    };
    let opts = prism_engine::Options {
        no_junk: false,
        state_dir: std::env::temp_dir().join("prism"),
        wintun_path: None,
        // routes are VpnService's business here
        smart: false,
        tun_fd,
    };
    let gen = CORE.generation.load(Ordering::SeqCst);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = watch::channel(false);
    let (done_tx, done_rx) = oneshot::channel();
    *CORE.running.lock() = Some(Running { stop: stop_tx, done: done_rx });
    let current = move || CORE.generation.load(Ordering::SeqCst) == gen;
    CORE.rt.spawn(async move {
        let mut engine = tokio::spawn(prism_engine::run(cfg, opts, ev_tx, stop_rx));
        let result = loop {
            tokio::select! {
                r = &mut engine => break r.unwrap_or_else(|e| Err(anyhow::anyhow!("the VPN session crashed ({e})"))),
                Some(e) = ev_rx.recv() => if current() { on_event(e) },
            }
        };
        if current() {
            let error = result.err().map(|e| {
                eprintln!("session ended: {e:#}");
                e.to_string()
            });
            *CORE.running.lock() = None;
            update(|s| *s = Snap { error, config_id: s.config_id.take(), name: s.name.take(), ..Default::default() });
        }
        let _ = done_tx.send(());
    });
}

fn on_event(e: Event) {
    match e {
        Event::Phase { phase } => update(|s| {
            s.error = None;
            s.state = match phase {
                Phase::Connecting => "connecting",
                Phase::Connected => {
                    s.since.get_or_insert_with(now_ms);
                    "connected"
                }
                Phase::Unreachable => "unreachable",
                Phase::Stalled => "stalled",
            };
        }),
        Event::Stats { rx, tx, handshake_age, level } => {
            {
                let mut s = CORE.snap.lock();
                s.rx = rx;
                s.tx = tx;
            }
            let line = serde_json::json!({ "t": "stats", "rx": rx, "tx": tx, "handshakeAge": handshake_age, "level": level });
            broadcast(line.to_string(), true);
        }
        Event::Log { line } => eprintln!("[prism] {line}"),
    }
}

/// The service couldn't bring the interface up.
pub fn failed(error: String) {
    CORE.pending.lock().take();
    CORE.generation.fetch_add(1, Ordering::SeqCst);
    update(|s| *s = Snap { error: Some(error), config_id: s.config_id.take(), name: s.name.take(), ..Default::default() });
}

fn begin_stop() -> Option<oneshot::Receiver<()>> {
    CORE.generation.fetch_add(1, Ordering::SeqCst);
    CORE.pending.lock().take();
    let running = CORE.running.lock().take();
    if is_running() {
        update(|s| *s = Snap { config_id: s.config_id.take(), name: s.name.take(), ..Default::default() });
    }
    running.map(|r| {
        let _ = r.stop.send(true);
        r.done
    })
}

/// Stops the session and waits for the engine to clean up (from a plain
/// thread, e.g. JNI).
pub fn stop_blocking() {
    if let Some(done) = begin_stop() {
        let (tx, rx) = std::sync::mpsc::channel();
        CORE.rt.spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(10), done).await;
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(Duration::from_secs(11));
    }
}

// ---------- PrismNative (Kotlin) ----------

mod jni_api {
    use jni::objects::{JClass, JString};
    use jni::sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE};
    use jni::JNIEnv;

    fn string(env: &mut JNIEnv, s: &JString) -> String {
        env.get_string(s).map(Into::into).unwrap_or_default()
    }

    fn jstr(env: &mut JNIEnv, s: String) -> jstring {
        env.new_string(s).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
    }

    fn jbool(b: bool) -> jboolean {
        if b {
            JNI_TRUE
        } else {
            JNI_FALSE
        }
    }

    /// The plan as JSON, or "!" + the reason it can't connect.
    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_plan(mut env: JNIEnv, _: JClass, dir: JString) -> jstring {
        let dir = string(&mut env, &dir);
        let out = match super::plan(&dir) {
            Ok(json) => json,
            Err(e) => {
                let msg = e.to_string();
                super::failed(msg.clone());
                format!("!{msg}")
            }
        };
        jstr(&mut env, out)
    }

    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_run(_env: JNIEnv, _: JClass, fd: jint) {
        super::run(fd);
    }

    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_failed(mut env: JNIEnv, _: JClass, msg: JString) {
        let msg = string(&mut env, &msg);
        super::failed(msg);
    }

    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_stop(_env: JNIEnv, _: JClass) {
        super::stop_blocking();
    }

    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_status(mut env: JNIEnv, _: JClass) -> jstring {
        let json = serde_json::to_string(&super::snapshot()).unwrap_or_default();
        jstr(&mut env, json)
    }

    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_isRunning(_env: JNIEnv, _: JClass) -> jboolean {
        jbool(super::is_running())
    }

    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_hasConfig(mut env: JNIEnv, _: JClass, dir: JString) -> jboolean {
        let dir = string(&mut env, &dir);
        jbool(super::has_config(&dir))
    }

    /// The service is up: let the window connect.
    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_serve(mut env: JNIEnv, _: JClass, dir: JString) {
        let dir = string(&mut env, &dir);
        super::serve(&dir);
    }

    #[no_mangle]
    pub extern "system" fn Java_app_prism_vpn_PrismNative_networkChanged(_env: JNIEnv, _: JClass) {
        prism_engine::net::network_changed();
    }
}

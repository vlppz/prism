//! Prism app: config list, one connect button, tray.

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
mod android_core;
#[cfg(any(target_os = "android", test))]
mod plan;
mod qr;
mod store;
mod vpn;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use prism_engine::{Event, Phase};
use serde::Serialize;
#[cfg(desktop)]
use tauri::image::Image;
#[cfg(desktop)]
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
#[cfg(target_os = "macos")]
use tauri::menu::Submenu;
#[cfg(desktop)]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
#[cfg(desktop)]
use tauri::{WindowEvent, Wry};
use tokio::sync::mpsc;

use store::{Index, Item, Store};
use vpn::{Session, Update};

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct Status {
    /// off | connecting | connected | unreachable | stalled
    state: &'static str,
    config_id: Option<String>,
    error: Option<String>,
    /// Unix ms when the tunnel first came up.
    since: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Stats {
    rx: u64,
    tx: u64,
    handshake_age: Option<u64>,
    level: u8,
}

struct AppState {
    store: Store,
    session: tokio::sync::Mutex<Option<Session>>,
    status: parking_lot::Mutex<Status>,
    generation: AtomicU64,
    #[cfg(desktop)]
    tray_toggle: parking_lot::Mutex<Option<MenuItem<Wry>>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn set_status(app: &AppHandle, f: impl FnOnce(&mut Status)) {
    let state = app.state::<AppState>();
    let snapshot = {
        let mut s = state.status.lock();
        f(&mut s);
        s.clone()
    };
    #[cfg(desktop)]
    {
        if let Some(item) = state.tray_toggle.lock().as_ref() {
            let _ = item.set_text(if snapshot.state == "off" { "Connect" } else { "Disconnect" });
        }
        set_tray_icon(app, snapshot.state == "connected");
    }
    let _ = app.emit("status", snapshot);
}

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    // anyhow's alternate form includes the cause chain
    e.to_string()
}

#[tauri::command]
fn list(state: State<'_, AppState>) -> Index {
    state.store.index()
}

#[tauri::command]
fn import_config(state: State<'_, AppState>, text: String, file_name: Option<String>) -> CmdResult<Item> {
    state.store.import(&text, file_name.as_deref()).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn remove(state: State<'_, AppState>, id: String) -> CmdResult<Index> {
    if state.status.lock().config_id.as_deref() == Some(&id) && state.status.lock().state != "off" {
        return Err("disconnect before removing this server".into());
    }
    state.store.remove(&id).map_err(err)
}

#[tauri::command]
fn select(state: State<'_, AppState>, id: String) -> CmdResult<Index> {
    state.store.select(&id).map_err(err)
}

#[tauri::command]
fn set_theme(app: AppHandle, state: State<'_, AppState>, theme: String) -> CmdResult<Index> {
    let index = state.store.set_theme(&theme).map_err(err)?;
    #[cfg(desktop)]
    if let Some(w) = app.get_webview_window("main") {
        apply_theme(&w, &theme);
    }
    let _ = &app;
    Ok(index)
}

const GITHUB: &str = "https://github.com/vlppz";

/// Opens the author's GitHub page in the default browser (a fixed URL, nothing from the page).
#[tauri::command]
fn open_github() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(GITHUB).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(GITHUB).spawn();
    #[cfg(target_os = "android")]
    android::open_url(GITHUB);
}

/// Turns smart routing on or off; a live tunnel reconnects to apply it.
#[tauri::command]
async fn set_smart(app: AppHandle, state: State<'_, AppState>, on: bool) -> CmdResult<Index> {
    let index = state.store.set_smart(on).map_err(err)?;
    if state.status.lock().state != "off" {
        do_disconnect(&app, &state).await;
        do_connect(&app, &state).await?;
    }
    Ok(index)
}

#[tauri::command]
async fn quit_app(app: AppHandle) {
    quit(app).await;
}

#[tauri::command]
fn details(state: State<'_, AppState>, id: String) -> CmdResult<store::Details> {
    state.store.details(&id).map_err(err)
}

#[tauri::command]
async fn server_ip(state: State<'_, AppState>, id: String) -> CmdResult<String> {
    state.store.server_ip(&id).await.map(|ip| ip.to_string()).map_err(err)
}

#[tauri::command]
fn rename(state: State<'_, AppState>, id: String, name: String) -> CmdResult<Index> {
    state.store.rename(&id, &name).map_err(err)
}

#[tauri::command]
fn set_country(state: State<'_, AppState>, id: String, country: String) -> CmdResult<Index> {
    state.store.set_country(&id, &country).map_err(err)
}

/// The text of the QR code in a picture (the page sends the file's bytes).
#[tauri::command]
async fn decode_qr(request: tauri::ipc::Request<'_>) -> CmdResult<String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected the picture's bytes".into());
    };
    let bytes = bytes.clone();
    tauri::async_runtime::spawn_blocking(move || qr::decode(&bytes)).await.map_err(err)?.map_err(err)
}

/// Android: scans a QR code with the camera; None if the user backed out.
#[tauri::command]
async fn scan_qr() -> CmdResult<Option<String>> {
    #[cfg(target_os = "android")]
    return android::scan_qr().await.map_err(err);
    #[cfg(not(target_os = "android"))]
    Err("not supported here".into())
}

/// Android: the Quick Settings tile opened Prism to connect.
#[tauri::command]
async fn take_connect() -> bool {
    #[cfg(target_os = "android")]
    return android::take_connect().await.unwrap_or(false);
    #[cfg(not(target_os = "android"))]
    false
}

/// Android: the clipboard's text (pages can't read it without a prompt).
#[tauri::command]
async fn paste_text() -> CmdResult<String> {
    #[cfg(target_os = "android")]
    return android::clipboard().await.map_err(err);
    #[cfg(not(target_os = "android"))]
    Err("not supported here".into())
}

/// Android: a config another app opened with Prism ("Open with", "Share"),
/// as (text, file name). Imported by the page like a dropped file.
#[tauri::command]
async fn take_opened() -> CmdResult<Option<(String, Option<String>)>> {
    #[cfg(target_os = "android")]
    {
        use base64::Engine;
        let o = android::take_opened().await.map_err(err)?;
        if let Some(b64) = o.image {
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(err)?;
            let text = tauri::async_runtime::spawn_blocking(move || qr::decode(&bytes)).await.map_err(err)?.map_err(err)?;
            // the picture's name says nothing about the server
            return Ok(Some((text, None)));
        }
        return Ok(o.text.filter(|t| !t.trim().is_empty()).map(|t| (t, o.name)));
    }
    #[cfg(not(target_os = "android"))]
    Ok(None)
}

#[tauri::command]
fn status(state: State<'_, AppState>) -> Status {
    state.status.lock().clone()
}

#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    do_connect(&app, &state).await
}

async fn do_connect(app: &AppHandle, state: &AppState) -> CmdResult<()> {
    // Android: the VPN service owns the session; status comes back through
    // the core's hook (see `run`)
    #[cfg(target_os = "android")]
    {
        if android_core::is_running() {
            return Ok(());
        }
        let index = state.store.index();
        let id = index.selected.ok_or("import a WireGuard config first")?;
        set_status(app, |s| *s = Status { state: "connecting", config_id: Some(id), ..Default::default() });
        let started = async {
            android::prepare().await?;
            android::connect().await
        };
        if let Err(e) = started.await {
            let msg = e.to_string();
            set_status(app, |s| *s = Status { state: "off", config_id: s.config_id.take(), error: Some(msg.clone()), since: None });
            return Err(msg);
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    let mut slot = state.session.lock().await;
    if slot.is_some() {
        return Ok(());
    }
    let index = state.store.index();
    let id = index.selected.ok_or("import a WireGuard config first")?;
    let text = state.store.read(&id).map_err(err)?;
    set_status(app, |s| *s = Status { state: "connecting", config_id: Some(id), ..Default::default() });

    let gen = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, rx) = mpsc::unbounded_channel();
    match Session::start(text, index.smart, tx).await {
        Ok(session) => {
            *slot = Some(session);
            tauri::async_runtime::spawn(pump(app.clone(), rx, gen));
            Ok(())
        }
        Err(e) => {
            // one clear sentence on screen; the chain behind it in the log
            eprintln!("connect failed: {e:#}");
            let msg = e.to_string();
            set_status(app, |s| *s = Status { state: "off", config_id: s.config_id.take(), error: Some(msg.clone()), since: None });
            Err(msg)
        }
    }
}

#[tauri::command]
async fn disconnect(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    do_disconnect(&app, &state).await;
    Ok(())
}

async fn do_disconnect(app: &AppHandle, state: &AppState) {
    #[cfg(target_os = "android")]
    {
        let _ = state;
        android_core::stop().await;
        set_status(app, |s| *s = Status { state: "off", config_id: s.config_id.take(), error: None, since: None });
        return;
    }
    #[allow(unreachable_code)]
    // a new generation makes the old session's late events irrelevant
    state.generation.fetch_add(1, Ordering::SeqCst);
    let session = state.session.lock().await.take();
    if let Some(s) = session {
        s.stop().await;
    }
    set_status(app, |s| *s = Status { state: "off", config_id: s.config_id.take(), error: None, since: None });
}

async fn pump(app: AppHandle, mut rx: mpsc::UnboundedReceiver<Update>, gen: u64) {
    while let Some(u) = rx.recv().await {
        let state = app.state::<AppState>();
        if state.generation.load(Ordering::SeqCst) != gen {
            return;
        }
        match u {
            Update::Event(Event::Phase { phase }) => set_status(&app, |s| {
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
            Update::Event(Event::Stats { rx, tx, handshake_age, level }) => {
                let _ = app.emit("stats", Stats { rx, tx, handshake_age, level });
            }
            Update::Event(Event::Log { line }) => eprintln!("[prism] {line}"),
            Update::Ended(error) => {
                state.session.lock().await.take();
                set_status(&app, |s| *s = Status { state: "off", config_id: s.config_id.take(), error, since: None });
                return;
            }
        }
    }
}

#[tauri::command]
fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "android") {
        "android"
    } else {
        "other"
    }
}

// Tray icon: a small triangle, outlined when off and filled when connected.
// macOS uses black template images (the menu bar tints them); Windows white.
#[cfg(target_os = "macos")]
const TRAY_OFF: &[u8] = include_bytes!("../icons/tray-mac-off.png");
#[cfg(target_os = "macos")]
const TRAY_ON: &[u8] = include_bytes!("../icons/tray-mac-on.png");
#[cfg(all(desktop, not(target_os = "macos")))]
const TRAY_OFF: &[u8] = include_bytes!("../icons/tray-win-off.png");
#[cfg(all(desktop, not(target_os = "macos")))]
const TRAY_ON: &[u8] = include_bytes!("../icons/tray-win-on.png");

#[cfg(desktop)]
fn set_tray_icon(app: &AppHandle, on: bool) {
    let Some(tray) = app.tray_by_id("main") else { return };
    if let Ok(img) = Image::from_bytes(if on { TRAY_ON } else { TRAY_OFF }) {
        let _ = tray.set_icon(Some(img));
        #[cfg(target_os = "macos")]
        let _ = tray.set_icon_as_template(true);
    }
    let _ = tray.set_tooltip(Some(if on { "Prism — connected" } else { "Prism" }));
}

/// Hides the window and (on macOS) the Dock icon; Prism keeps running in the tray.
#[cfg(desktop)]
fn hide_to_tray(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
}

#[cfg(desktop)]
fn show_main(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

async fn quit(app: AppHandle) {
    let state = app.state::<AppState>();
    do_disconnect(&app, &state).await;
    app.exit(0);
}

#[cfg(desktop)]
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Connect", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Show Prism", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit Prism", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &show, &PredefinedMenuItem::separator(app)?, &quit_item])?;
    *app.state::<AppState>().tray_toggle.lock() = Some(toggle);

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Prism")
        .menu(&menu)
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, e| match e.id.as_ref() {
            "toggle" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    if state.status.lock().state == "off" {
                        let _ = do_connect(&app, &state).await;
                    } else {
                        do_disconnect(&app, &state).await;
                    }
                });
            }
            "show" => show_main(app),
            "quit" => {
                tauri::async_runtime::spawn(quit(app.clone()));
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                if !cfg!(target_os = "macos") {
                    show_main(tray.app_handle());
                }
            }
        });
    tray = tray.icon(Image::from_bytes(TRAY_OFF)?);
    #[cfg(target_os = "macos")]
    {
        tray = tray.icon_as_template(true);
    }
    tray.build(app)?;
    Ok(())
}

/// macOS app menu. Cmd+Q hides Prism to the tray instead of quitting; the
/// tray's "Quit Prism" quits for real. The Edit menu keeps copy/paste working
/// (pasting a config imports it).
#[cfg(target_os = "macos")]
fn app_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let hide = MenuItem::with_id(app, "hide-to-tray", "Hide Prism to Tray", true, Some("CmdOrCtrl+Q"))?;
    let quit = MenuItem::with_id(app, "quit-all", "Quit Prism", true, Some("CmdOrCtrl+Alt+Q"))?;
    let close = MenuItem::with_id(app, "hide-to-tray-w", "Close Window", true, Some("CmdOrCtrl+W"))?;
    let app_sub = Submenu::with_items(
        app,
        "Prism",
        true,
        &[
            &PredefinedMenuItem::about(
                app,
                Some("About Prism"),
                Some(tauri::menu::AboutMetadata {
                    credits: Some(format!("WireGuard that gets through.\n{}", GITHUB.trim_start_matches("https://"))),
                    ..Default::default()
                }),
            )?,
            &PredefinedMenuItem::separator(app)?,
            &hide,
            &quit,
        ],
    )?;
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    let window = Submenu::with_items(app, "Window", true, &[&PredefinedMenuItem::minimize(app, None)?, &close])?;
    Menu::with_items(app, &[&app_sub, &edit, &window])
}

/// The native window colour under the page, matched to the appearance so
/// launch and resizes never flash the other theme.
/// Pins the window (and so the page's prefers-color-scheme) to a theme, or
/// hands it back to the system.
#[cfg(desktop)]
fn apply_theme(w: &tauri::WebviewWindow, theme: &str) {
    let forced = match theme {
        "light" => Some(tauri::Theme::Light),
        "dark" => Some(tauri::Theme::Dark),
        _ => None,
    };
    let _ = w.set_theme(forced);
    let now = forced.or_else(|| w.theme().ok()).unwrap_or(tauri::Theme::Dark);
    let _ = w.set_background_color(Some(background(now)));
}

#[cfg(desktop)]
fn background(theme: tauri::Theme) -> tauri::window::Color {
    match theme {
        tauri::Theme::Light => tauri::window::Color(243, 243, 246, 255),
        _ => tauri::window::Color(5, 5, 7, 255),
    }
}

/// Android: mirrors the session the VPN service runs (it may have started
/// from the tile, or before this window existed).
#[cfg(target_os = "android")]
fn follow_android_session(app: &AppHandle) {
    fn apply(app: &AppHandle, snap: android_core::Snap) {
        set_status(app, |s| {
            *s = Status { state: snap.state, config_id: snap.config_id, error: snap.error, since: snap.since }
        });
    }
    let handle = app.clone();
    android_core::set_hook(move |note| match note {
        android_core::Note::Status(snap) => apply(&handle, snap),
        android_core::Note::Stats { rx, tx, handshake_age, level } => {
            let _ = handle.emit("stats", Stats { rx, tx, handshake_age, level });
        }
    });
    let snap = android_core::snapshot();
    if snap.state != "off" {
        apply(app, snap);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    // The app menu exists only on macOS (on Windows it would draw a menu bar).
    #[cfg(target_os = "macos")]
    let builder = builder.menu(app_menu);
    #[cfg(desktop)]
    let builder = builder
        .on_menu_event(|app, e| match e.id.as_ref() {
            "hide-to-tray" | "hide-to-tray-w" => hide_to_tray(app),
            "quit-all" => {
                tauri::async_runtime::spawn(quit(app.clone()));
            }
            _ => {}
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .on_window_event(|window, event| match event {
            // Closing the window keeps Prism (and the tunnel) running in the tray.
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                hide_to_tray(window.app_handle());
            }
            WindowEvent::ThemeChanged(theme) => {
                let _ = window.set_background_color(Some(background(*theme)));
            }
            _ => {}
        });
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android::init());
    let app = builder
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            let store = Store::open(dir)?;
            app.manage(AppState {
                store,
                session: Default::default(),
                status: parking_lot::Mutex::new(Status { state: "off", ..Default::default() }),
                generation: AtomicU64::new(0),
                #[cfg(desktop)]
                tray_toggle: Default::default(),
            });
            #[cfg(target_os = "android")]
            follow_android_session(app.handle());
            #[cfg(desktop)]
            {
                setup_tray(app.handle())?;
                if let Some(w) = app.get_webview_window("main") {
                    apply_theme(&w, &app.state::<AppState>().store.index().theme);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list,
            import_config,
            remove,
            select,
            status,
            connect,
            disconnect,
            platform,
            set_theme,
            quit_app,
            details,
            open_github,
            set_smart,
            server_ip,
            set_country,
            paste_text,
            take_opened,
            decode_qr,
            scan_qr,
            take_connect,
            rename
        ])
        .build(tauri::generate_context!())
        .expect("error while building Prism");

    app.run(|app, event| match event {
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main(app),
        RunEvent::ExitRequested { api, code, .. } => {
            // System quit (logout, shutdown, Dock → Quit): never block it, but
            // take the tunnel down cleanly first. (Cmd+Q only hides, see app_menu.)
            let state = app.state::<AppState>();
            if code.is_none() && state.status.lock().state != "off" {
                api.prevent_exit();
                tauri::async_runtime::spawn(quit(app.clone()));
            }
        }
        _ => {}
    });
}

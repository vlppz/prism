//! Android: what the open app asks of the Kotlin side (VpnPlugin.kt): the
//! VPN permission, starting the VPN service, imports, the tile. The session
//! itself lives in [`crate::android_core`], driven by PrismVpnService.kt.

use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::Wry;

static PLUGIN: OnceLock<PluginHandle<Wry>> = OnceLock::new();

pub fn init() -> TauriPlugin<Wry> {
    Builder::new("prism-vpn")
        .setup(|app, api| {
            let handle = api.register_android_plugin("app.prism.vpn", "VpnPlugin")?;
            let _ = PLUGIN.set(handle);
            let _ = app;
            Ok(())
        })
        .build()
}

fn plugin() -> Result<&'static PluginHandle<Wry>> {
    PLUGIN.get().ok_or_else(|| anyhow!("Prism's VPN service isn't ready yet"))
}

/// Runs a Kotlin command. Commands run through the app's window, so when
/// Android has closed it (the tunnel can outlive it) a call never returns:
/// the timeout keeps a session from waiting forever.
async fn call<T: serde::de::DeserializeOwned>(cmd: &str, payload: impl serde::Serialize, secs: u64) -> Result<T> {
    let p = plugin()?;
    match tokio::time::timeout(std::time::Duration::from_secs(secs), p.run_mobile_plugin_async(cmd, payload)).await {
        Ok(r) => r.map_err(|e| anyhow!("{e}")),
        Err(_) => Err(anyhow!("Prism's window didn't respond. Open Prism and try again")),
    }
}

#[derive(Deserialize)]
struct Granted {
    granted: bool,
}

/// Shows Android's "connection request" dialog the first time.
pub async fn prepare() -> Result<()> {
    // the user may take a while with the system dialog
    let r: Granted = call("prepare", (), 600).await?;
    if !r.granted {
        bail!("Prism needs permission to set up a VPN connection");
    }
    Ok(())
}

/// Starts the VPN service, which connects to the selected server; progress
/// arrives through the session core.
pub async fn connect() -> Result<()> {
    call::<serde_json::Value>("connect", (), 15).await.map(drop)
}

pub fn open_url(url: &str) {
    if let Ok(p) = plugin() {
        let _ = p.run_mobile_plugin::<serde_json::Value>("openUrl", serde_json::json!({ "url": url }));
    }
}

#[derive(Deserialize)]
struct Text {
    text: String,
}

pub async fn clipboard() -> Result<String> {
    let r: Text = call("clipboard", (), 10).await?;
    Ok(r.text)
}

/// A config opened with Prism from another app, if one is waiting.
#[derive(Deserialize, serde::Serialize)]
pub struct Opened {
    pub text: Option<String>,
    pub name: Option<String>,
    /// A shared picture (base64), presumably of a QR code.
    pub image: Option<String>,
}

pub async fn take_opened() -> Result<Opened> {
    call("takeOpened", (), 30).await
}

#[derive(Deserialize)]
struct Scanned {
    text: Option<String>,
}

/// Opens the camera scanner; None if the user backed out.
pub async fn scan_qr() -> Result<Option<String>> {
    let r: Scanned = call("scanQr", (), 3600).await?;
    Ok(r.text)
}

#[derive(Deserialize)]
struct Connect {
    connect: bool,
}

/// Whether the Quick Settings tile opened Prism to connect.
pub async fn take_connect() -> Result<bool> {
    Ok(call::<Connect>("takeConnect", (), 10).await?.connect)
}

#[derive(Deserialize)]
struct AddTile {
    result: String,
}

/// added | already | declined | manual (Android 12 and older: no prompt).
pub async fn add_tile() -> Result<String> {
    Ok(call::<AddTile>("addTile", (), 120).await?.result)
}

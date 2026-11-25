fn main() {
    // Windows: the tunnel runs inside the app, which needs administrator
    // rights (one UAC prompt at launch, no background service).
    let attrs = tauri_build::Attributes::new().windows_attributes(
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("windows-app-manifest.xml")),
    );
    tauri_build::try_build(attrs).expect("tauri build");
}

//! `prism-helper` — the only part of Prism that runs as root on macOS
//! (creating a TUN device and changing routes/DNS needs it).
//!
//! * `prism-helper install --uid <uid>`  copy itself to a root-owned dir and
//!   register a launchd daemon (run once through an admin prompt)
//! * `prism-helper daemon --uid <uid>`   the daemon: serves only that uid
//! * `prism-helper uninstall`
//! * `prism-helper run <file.conf> [--no-junk]`  foreground debug session

#[cfg(unix)]
mod daemon;

use anyhow::{bail, Context, Result};

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match rt.block_on(run()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("prism-helper: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

async fn run() -> Result<()> {
    let mut argv = std::env::args().skip(1);
    let cmd = argv.next().unwrap_or_default();
    match cmd.as_str() {
        #[cfg(unix)]
        "daemon" => daemon::serve(daemon::uid_arg(argv)?).await,
        #[cfg(unix)]
        "install" => daemon::install(daemon::uid_arg(argv)?),
        #[cfg(unix)]
        "uninstall" => daemon::uninstall(),
        "run" => foreground(argv).await,
        "--version" => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("unknown command {other:?}"),
    }
}

/// Debug mode: run one session in the foreground until Ctrl-C.
async fn foreground(mut argv: impl Iterator<Item = String>) -> Result<()> {
    let path = argv.next().context("usage: prism-helper run <file.conf> [--no-junk] [--smart]")?;
    let flags: Vec<String> = argv.collect();
    let no_junk = flags.iter().any(|a| a == "--no-junk");
    let smart = flags.iter().any(|a| a == "--smart");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let cfg = prism_engine::parse(&text)?;
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        while let Some(e) = ev_rx.recv().await {
            match e {
                prism_engine::Event::Log { .. } => {} // already printed to stderr
                other => println!("{}", serde_json::to_string(&other).unwrap_or_default()),
            }
        }
    });
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = stop_tx.send(true);
    });
    let opts = prism_engine::Options {
        no_junk,
        state_dir: std::env::temp_dir().join("prism-helper-run"),
        wintun_path: None,
        smart,
    };
    prism_engine::run(cfg, opts, ev_tx, stop_rx).await
}

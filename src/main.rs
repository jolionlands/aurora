use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use aurora::config::parse::{default_config_path, parse_kdl_config};
use aurora::ipc::IpcServer;

/// Bundled default configuration — written to disk on first run.
const DEFAULT_CONFIG_KDL: &str = include_str!("../resources/default_config.kdl");

#[tokio::main]
async fn main() -> Result<()> {
    // ---------------------------------------------------------------------------
    // 1. Tracing / logging
    // ---------------------------------------------------------------------------
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env())
        .init();

    info!("aurora 0.1.0 starting");

    // ---------------------------------------------------------------------------
    // 2. COM initialisation (required for WIC + IDesktopWallpaper)
    // ---------------------------------------------------------------------------
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        // S_FALSE (already initialised on this thread) is also acceptable.
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            return Err(anyhow::anyhow!("CoInitializeEx failed: {:?}", hr));
        }
    }

    // ---------------------------------------------------------------------------
    // 3. Single-instance check
    //    Try to open the pipe as a CLIENT. If it succeeds, another aurora is
    //    already running.
    // ---------------------------------------------------------------------------
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        if ClientOptions::new().open(aurora::ipc::PIPE_PATH).is_ok() {
            eprintln!("aurora: another instance is already running (pipe open at {})", aurora::ipc::PIPE_PATH);
            std::process::exit(1);
        }
    }

    // ---------------------------------------------------------------------------
    // 4. Load config — write default on first run
    // ---------------------------------------------------------------------------
    let config_path = default_config_path();
    if !config_path.exists() {
        info!("Writing default config to {}", config_path.display());
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        std::fs::write(&config_path, DEFAULT_CONFIG_KDL)
            .with_context(|| format!("write default config to {}", config_path.display()))?;
    }

    let config_src = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read config {}", config_path.display()))?;
    let _config = parse_kdl_config(&config_src)
        .with_context(|| format!("parse config {}", config_path.display()))?;

    info!("Config loaded from {}", config_path.display());

    // ---------------------------------------------------------------------------
    // 5. Start IPC server
    // ---------------------------------------------------------------------------
    let ipc = Arc::new(IpcServer::new());

    // Wire up a shutdown channel so `aurora-ctl quit` can signal us cleanly.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    ipc.set_shutdown_sender(shutdown_tx);

    let ipc_server = Arc::clone(&ipc);
    tokio::spawn(async move {
        if let Err(e) = ipc_server.run().await {
            tracing::error!("IPC server exited with error: {}", e);
        }
    });

    info!("aurora ready. IPC pipe: {}", aurora::ipc::PIPE_PATH);

    // ---------------------------------------------------------------------------
    // 6. Main loop — wait for shutdown signal
    //    (Real scheduler / watcher arrives in round 2)
    // ---------------------------------------------------------------------------
    let _ = shutdown_rx.await;
    info!("Shutdown signal received — exiting");

    Ok(())
}

#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use aurora::apply::WallpaperApplier;
use aurora::config::parse::{default_config_path, parse_kdl_config};
use aurora::hooks::StartupManager;
use aurora::integrations::wiri::subscribe_wiri_events;
use aurora::ipc::IpcServer;
use aurora::metrics::{serve_metrics, Metrics};
use aurora::playlist::default_playlists_path;
use aurora::runtime::{Runtime, RuntimeHandle, RuntimeStateSnapshot};
use aurora::scheduler::Scheduler;

/// Bundled default configuration — written to disk on first run.
const DEFAULT_CONFIG_KDL: &str = include_str!("../resources/default_config.kdl");

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "aurora",
    about = "Wallpaper cycling daemon for Windows",
    version
)]
struct Args {
    /// Register aurora in the Windows Run key so it starts with Windows.
    #[arg(long)]
    register_autostart: bool,

    /// Remove aurora from the Windows Run key.
    #[arg(long)]
    unregister_autostart: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    enable_dpi_awareness();

    // ---------------------------------------------------------------------------
    // 1. Tracing / logging
    // ---------------------------------------------------------------------------
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env())
        .init();

    info!("aurora 0.1.0 starting");

    // ---------------------------------------------------------------------------
    // 1a. Autostart flags — handle BEFORE single-instance check or IPC startup
    // ---------------------------------------------------------------------------
    let args = Args::parse();

    if args.register_autostart {
        let mgr = StartupManager::new();
        match mgr.register() {
            Ok(()) => {
                info!(
                    "autostart: registered aurora in Windows Run key ({})",
                    mgr.get_registered_path().unwrap_or_default()
                );
                println!("aurora registered for autostart.");
            }
            Err(e) => {
                eprintln!("aurora: failed to register autostart: {}", e);
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if args.unregister_autostart {
        let mgr = StartupManager::new();
        match mgr.unregister() {
            Ok(()) => {
                info!("autostart: removed aurora from Windows Run key");
                println!("aurora removed from autostart.");
            }
            Err(e) => {
                eprintln!("aurora: failed to unregister autostart: {}", e);
                std::process::exit(1);
            }
        }
        return Ok(());
    }

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
            eprintln!(
                "aurora: another instance is already running (pipe open at {})",
                aurora::ipc::PIPE_PATH
            );
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
    let config = parse_kdl_config(&config_src)
        .with_context(|| format!("parse config {}", config_path.display()))?;

    info!("Config loaded from {}", config_path.display());

    // Log autostart status.
    {
        let mgr = StartupManager::new();
        if mgr.is_registered() {
            info!(
                "autostart: registered (path: {})",
                mgr.get_registered_path().unwrap_or_default()
            );
        } else {
            info!("autostart: not registered (run with --register-autostart to enable)");
        }
    }

    // ---------------------------------------------------------------------------
    // 5. Build metrics, applier, runtime
    // ---------------------------------------------------------------------------
    let metrics = Metrics::new();

    let applier = WallpaperApplier::new().context("create WallpaperApplier")?;

    let mut runtime =
        Runtime::new(&config, applier, Arc::clone(&metrics)).context("initialise Runtime")?;

    // ---------------------------------------------------------------------------
    // 6. Scheduler — owns the swap channel sender
    // ---------------------------------------------------------------------------
    let (scheduler, swap_rx) = Scheduler::new(config.schedule.clone());
    let scheduler = Arc::new(scheduler);

    // Clone the sender so both wiri integration and the runtime share it.
    // We derive a sender clone from a new unbounded channel that feeds into
    // the same receiver as the scheduler's channel — instead, we expose
    // the scheduler's channel sender via a thin wrapper.
    //
    // Actually: the scheduler owns swap_tx internally; wiri sends directly
    // into it via on_workspace_change(), and we need the raw tx for RuntimeHandle.
    // Re-create a separate channel so Runtime and wiri can also inject requests.
    let (extra_tx, extra_rx) =
        tokio::sync::mpsc::unbounded_channel::<aurora::scheduler::SwapRequest>();

    // Merge: forward scheduler rx + extra rx into a single merged rx.
    // Use a simple select loop in a spawned task.
    let (merged_tx, merged_rx) =
        tokio::sync::mpsc::unbounded_channel::<aurora::scheduler::SwapRequest>();

    {
        let mtx = merged_tx.clone();
        tokio::spawn(async move {
            let mut swap_rx = swap_rx;
            while let Some(req) = swap_rx.recv().await {
                if mtx.send(req).is_err() {
                    break;
                }
            }
        });
    }
    {
        let mtx = merged_tx.clone();
        tokio::spawn(async move {
            let mut extra_rx = extra_rx;
            while let Some(req) = extra_rx.recv().await {
                if mtx.send(req).is_err() {
                    break;
                }
            }
        });
    }

    // Build shared snapshot state for IPC queries.
    let snap_state = Arc::new(parking_lot::Mutex::new(RuntimeStateSnapshot {
        paused: false,
        current_path: std::collections::HashMap::new(),
        history_len: 0,
        history: std::collections::VecDeque::new(),
    }));

    // Build RuntimeHandle (for IPC).
    let runtime_handle = RuntimeHandle::new(
        extra_tx.clone(),
        Arc::clone(&snap_state),
        runtime.index_arc(),
        Arc::clone(&metrics),
        config_path.clone(),
        runtime.playlist_arc(),
        default_playlists_path(),
    );

    // Extract the shared pause Arc so Runtime::run can check IPC pause state.
    let pause_arc = runtime_handle.pause_arc();

    // ---------------------------------------------------------------------------
    // 7. Start IPC server
    // ---------------------------------------------------------------------------
    let ipc = Arc::new(IpcServer::new());
    ipc.set_runtime(runtime_handle.clone());

    // Wire IPC broadcast sender into Runtime so it can emit WallpaperChanged events.
    runtime.set_event_sender(ipc.event_tx_clone());

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
    // 8. Start scheduler
    // ---------------------------------------------------------------------------
    tokio::spawn({
        let s = Arc::clone(&scheduler);
        async move {
            s.run().await;
        }
    });

    // ---------------------------------------------------------------------------
    // 9. Wiri integration (optional)
    // ---------------------------------------------------------------------------
    if config.schedule.on_workspace_change {
        let tx = extra_tx.clone();
        tokio::spawn(async move {
            let _ = subscribe_wiri_events(tx).await;
        });
    }

    // ---------------------------------------------------------------------------
    // 10. Metrics HTTP server (optional)
    // ---------------------------------------------------------------------------
    if config.metrics.enabled {
        let port = config.metrics.port;
        let m = Arc::clone(&metrics);
        tokio::spawn(async move {
            if let Err(e) = serve_metrics(port, m).await {
                tracing::warn!("metrics server: {}", e);
            }
        });
    }

    // ---------------------------------------------------------------------------
    // 11. Runtime drain loop
    // ---------------------------------------------------------------------------
    tokio::spawn(async move {
        runtime.run(merged_rx, snap_state, pause_arc).await;
    });

    // ---------------------------------------------------------------------------
    // 12. Main loop — wait for shutdown signal
    // ---------------------------------------------------------------------------
    let _ = shutdown_rx.await;
    info!("Shutdown signal received — exiting");

    Ok(())
}

#[cfg(target_os = "windows")]
fn enable_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    // Must happen before Aurora creates transition HWNDs or queries monitor geometry.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

#[cfg(not(target_os = "windows"))]
fn enable_dpi_awareness() {}

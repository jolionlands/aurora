#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{Context, Result};
use clap::Parser;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use aurora::apply::WallpaperApplier;
use aurora::config::parse::{default_config_path, parse_kdl_config};
use aurora::hooks::StartupManager;
use aurora::integrations::wiri::subscribe_wiri_events;
use aurora::ipc::IpcServer;
use aurora::metrics::{serve_metrics, Metrics};
use aurora::playlist::default_playlists_path;
use aurora::runtime::{ComApartment, Runtime, RuntimeHandle, RuntimeStateSnapshot};
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
    #[arg(long, hide = true, value_name = "PATH")]
    apply_once: Option<PathBuf>,

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
    // 1. CLI flags that do not start the daemon
    // ---------------------------------------------------------------------------
    let args = Args::parse();
    if let Some(path) = args.apply_once {
        return apply_once(&path);
    }
    if args.register_autostart || args.unregister_autostart {
        init_logging("info")?;
    }

    if args.register_autostart {
        let mgr = StartupManager::default();
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
        let mgr = StartupManager::default();
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
    // 2. Reserve this session's authoritative IPC endpoint before startup work.
    // ---------------------------------------------------------------------------
    let _singleton = aurora::ipc::SingletonGuard::acquire()?;
    let ipc = Arc::new(IpcServer::bind()?);

    // ---------------------------------------------------------------------------
    // 3. COM initialisation (required for WIC + IDesktopWallpaper)
    // ---------------------------------------------------------------------------
    let _com = ComApartment::initialize()?;

    // ---------------------------------------------------------------------------
    // 4. Load config — write default on first run
    // ---------------------------------------------------------------------------
    let config_path = default_config_path();
    let wrote_default_config = !config_path.exists();
    if wrote_default_config {
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

    init_logging(&config.log_level)?;
    info!("aurora 0.1.0 starting");
    if wrote_default_config {
        info!("Wrote default config to {}", config_path.display());
    }
    info!("Config loaded from {}", config_path.display());

    // Log autostart status.
    {
        let mgr = StartupManager::default();
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

    let mut runtime = Runtime::new(&config, &config_path, applier, Arc::clone(&metrics))
        .context("initialise Runtime")?;

    // ---------------------------------------------------------------------------
    // 6. Scheduler — owns the swap channel sender
    // ---------------------------------------------------------------------------
    let (scheduler, swap_rx) = Scheduler::new(config.schedule.clone());
    let swap_tx = scheduler.sender();
    let scheduler = Arc::new(scheduler);

    // Build shared snapshot state for IPC queries.
    let snap_state = Arc::new(parking_lot::Mutex::new(RuntimeStateSnapshot::default()));

    // Build RuntimeHandle (for IPC).
    let runtime_handle = RuntimeHandle::new(
        swap_tx.clone(),
        Arc::clone(&snap_state),
        (
            runtime.index_arc(),
            runtime.source_roots_arc(),
            runtime.ban_gate(),
        ),
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
    ipc.set_runtime(runtime_handle.clone());

    // Wire IPC broadcast sender into Runtime so it can emit WallpaperChanged events.
    runtime.set_event_sender(ipc.event_tx_clone());

    // Wire up a shutdown channel so `aurora-ctl quit` can signal us cleanly.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    ipc.set_shutdown_sender(shutdown_tx);

    let ipc_server = Arc::clone(&ipc);
    let ipc_task = tokio::spawn(async move { ipc_server.run().await });

    info!("aurora ready. IPC pipe: {}", aurora::ipc::pipe_path()?);

    // ---------------------------------------------------------------------------
    // 8. Start scheduler
    // ---------------------------------------------------------------------------
    let scheduler_task = tokio::spawn({
        let s = Arc::clone(&scheduler);
        async move {
            s.run().await;
        }
    });

    // ---------------------------------------------------------------------------
    // 9. Wiri integration (optional)
    // ---------------------------------------------------------------------------
    if config.schedule.on_workspace_change {
        let tx = swap_tx;
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
    // Runtime owns apartment-bound COM interfaces, so poll it on the root
    // thread where COM was initialized rather than on a Tokio worker.
    wait_for_core_exit(
        runtime.run(swap_rx, snap_state, pause_arc),
        shutdown_rx,
        ipc_task,
        scheduler_task,
    )
    .await
}

fn apply_once(path: &Path) -> Result<()> {
    let _com = ComApartment::initialize()?;
    let config_path = default_config_path();
    let config = parse_kdl_config(
        &std::fs::read_to_string(&config_path)
            .with_context(|| format!("read config {}", config_path.display()))?,
    )
    .with_context(|| format!("parse config {}", config_path.display()))?;
    WallpaperApplier::new()?.apply_all(&config, path)
}

fn init_logging(default_filter: &str) -> Result<()> {
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value).context("invalid RUST_LOG filter")?,
        Err(std::env::VarError::NotPresent) => EnvFilter::try_new(default_filter)
            .with_context(|| format!("invalid config log-level {default_filter:?}"))?,
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("RUST_LOG is not valid Unicode")
        }
    };
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(filter)
        .try_init()
        .context("initialize logging")
}

async fn wait_for_core_exit(
    runtime: impl Future<Output = ()>,
    mut shutdown_rx: oneshot::Receiver<()>,
    mut ipc_task: JoinHandle<Result<()>>,
    mut scheduler_task: JoinHandle<()>,
) -> Result<()> {
    tokio::pin!(runtime);
    tokio::select! {
        result = &mut shutdown_rx => {
            abort_and_join(&mut ipc_task).await;
            abort_and_join(&mut scheduler_task).await;
            match result {
                Ok(()) => {
                    info!("Shutdown signal received — exiting");
                    Ok(())
                }
                Err(error) => Err(anyhow::anyhow!(
                    "shutdown channel closed unexpectedly: {error}"
                )),
            }
        }
        _ = &mut runtime => {
            abort_and_join(&mut ipc_task).await;
            abort_and_join(&mut scheduler_task).await;
            Err(anyhow::anyhow!("runtime exited unexpectedly"))
        }
        result = &mut ipc_task => {
            abort_and_join(&mut scheduler_task).await;
            match result {
                Ok(Ok(())) => Err(anyhow::anyhow!("IPC server exited unexpectedly")),
                Ok(Err(error)) => Err(anyhow::anyhow!("IPC server failed: {error:#}")),
                Err(error) => Err(anyhow::anyhow!("IPC server task failed: {error}")),
            }
        }
        result = &mut scheduler_task => {
            abort_and_join(&mut ipc_task).await;
            match result {
                Ok(()) => Err(anyhow::anyhow!("scheduler exited unexpectedly")),
                Err(error) => Err(anyhow::anyhow!("scheduler task failed: {error}")),
            }
        }
    }
}

async fn abort_and_join<T>(task: &mut JoinHandle<T>) {
    task.abort();
    let _ = task.await;
}

fn enable_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    // Must happen before Aurora creates transition HWNDs or queries monitor geometry.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ipc_exit_wakes_supervisor() {
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let ipc_task = tokio::spawn(async { Err(anyhow::anyhow!("pipe failed")) });
        let scheduler_task = tokio::spawn(std::future::pending::<()>());

        let error = wait_for_core_exit(
            std::future::pending(),
            shutdown_rx,
            ipc_task,
            scheduler_task,
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "IPC server failed: pipe failed");
    }

    #[tokio::test]
    async fn scheduler_exit_wakes_supervisor() {
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let ipc_task = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok(())
        });
        let scheduler_task = tokio::spawn(async {});

        let error = wait_for_core_exit(
            std::future::pending(),
            shutdown_rx,
            ipc_task,
            scheduler_task,
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "scheduler exited unexpectedly");
    }

    #[tokio::test]
    async fn runtime_exit_wakes_supervisor() {
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let ipc_task = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok(())
        });
        let scheduler_task = tokio::spawn(std::future::pending::<()>());

        let error = wait_for_core_exit(async {}, shutdown_rx, ipc_task, scheduler_task)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "runtime exited unexpectedly");
    }

    #[tokio::test]
    async fn shutdown_signal_exits_cleanly() {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let ipc_task = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok(())
        });
        let scheduler_task = tokio::spawn(std::future::pending::<()>());
        shutdown_tx.send(()).unwrap();

        wait_for_core_exit(
            std::future::pending(),
            shutdown_rx,
            ipc_task,
            scheduler_task,
        )
        .await
        .unwrap();
    }
}

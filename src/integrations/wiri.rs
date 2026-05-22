/// Wiri IPC integration — subscribes to workspace_switched events.
///
/// Connects to \\.\pipe\wiri_control, sends a subscribe message, and
/// forwards workspace-change events to the scheduler's swap channel.
///
/// If wiri is not running, retries every 30 seconds.
use anyhow::Result;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::scheduler::{SwapRequest, SwapReason};

const PIPE_NAME: &str = r"\\.\pipe\wiri_control";
const RETRY_SECS: u64 = 30;

/// Subscribe to wiri workspace events and forward them as SwapRequests.
///
/// Spawns in the background — never returns under normal operation.
pub async fn subscribe_wiri_events(
    swap_tx: mpsc::UnboundedSender<SwapRequest>,
) -> Result<()> {
    loop {
        match try_connect_and_subscribe(&swap_tx).await {
            Ok(()) => {
                // Connection closed cleanly — reconnect
                tracing::info!("wiri IPC connection closed, reconnecting in {RETRY_SECS}s");
            }
            Err(e) => {
                tracing::debug!("wiri IPC unavailable ({e}), retrying in {RETRY_SECS}s");
            }
        }

        tokio::time::sleep(Duration::from_secs(RETRY_SECS)).await;
    }
}

async fn try_connect_and_subscribe(
    swap_tx: &mpsc::UnboundedSender<SwapRequest>,
) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        connect_windows(swap_tx).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        // On non-Windows, we simply wait indefinitely (no wiri)
        let _ = swap_tx;
        futures::future::pending::<()>().await;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
async fn connect_windows(
    swap_tx: &mpsc::UnboundedSender<SwapRequest>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut pipe = ClientOptions::new()
        .open(PIPE_NAME)
        .map_err(|e| anyhow::anyhow!("named pipe open failed: {e}"))?;

    tracing::info!("connected to wiri IPC at {PIPE_NAME}");

    // Send subscription request
    let subscribe_msg = serde_json::json!({
        "type": "subscribe_events",
        "event_types": ["workspace_switched"]
    });
    let mut msg_bytes = serde_json::to_vec(&subscribe_msg)?;
    msg_bytes.push(b'\n');
    pipe.write_all(&msg_bytes).await?;

    // Read events line by line
    let reader = BufReader::new(&mut pipe);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        tracing::trace!("wiri event: {line}");

        match parse_event(&line) {
            Some(WiriEvent::WorkspaceSwitched) => {
                if swap_tx
                    .send(SwapRequest {
                        reason: SwapReason::WorkspaceChange,
                        specific: None,
                    })
                    .is_err()
                {
                    // Receiver dropped — daemon is shutting down
                    break;
                }
            }
            None => {
                // Unknown or irrelevant event
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Event parsing
// ---------------------------------------------------------------------------

enum WiriEvent {
    WorkspaceSwitched,
}

fn parse_event(line: &str) -> Option<WiriEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?;
    match event_type {
        "workspace_switched" | "WorkspaceSwitched" => Some(WiriEvent::WorkspaceSwitched),
        _ => None,
    }
}

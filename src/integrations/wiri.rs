/// Wiri IPC integration — subscribes to workspace_switched events.
///
/// Connects to \\.\pipe\wiri_control, sends a subscribe message, and
/// forwards workspace-change events to the scheduler's swap channel.
///
/// If wiri is not running, retries every 30 seconds.
use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::mpsc;

use crate::scheduler::{SwapReason, SwapRequest};

const PIPE_NAME: &str = r"\\.\pipe\wiri_control";
const RETRY_SECS: u64 = 30;
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Subscribe to wiri workspace events and forward them as SwapRequests.
///
/// Spawns in the background — never returns under normal operation.
pub async fn subscribe_wiri_events(swap_tx: mpsc::Sender<SwapRequest>) -> Result<()> {
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

async fn try_connect_and_subscribe(swap_tx: &mpsc::Sender<SwapRequest>) -> Result<()> {
    connect_windows(swap_tx).await
}

async fn connect_windows(swap_tx: &mpsc::Sender<SwapRequest>) -> Result<()> {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut pipe = ClientOptions::new()
        .open(PIPE_NAME)
        .map_err(|e| anyhow::anyhow!("named pipe open failed: {e}"))?;
    verify_server_session(&pipe)?;

    tracing::info!("connected to wiri IPC at {PIPE_NAME}");

    // Send subscription request
    let subscribe_msg = serde_json::json!({
        "type": "subscribe_events",
        "data": {
            "event_types": ["workspace_switched"]
        }
    });
    let mut msg_bytes = serde_json::to_vec(&subscribe_msg)?;
    msg_bytes.push(b'\n');
    pipe.write_all(&msg_bytes).await?;

    // Read events line by line
    let mut reader = BufReader::new(pipe);
    let ack = read_bounded_line(&mut reader, MAX_LINE_BYTES)
        .await?
        .ok_or_else(|| anyhow::anyhow!("wiri closed before subscription acknowledgement"))?;
    verify_subscription_ack(&ack)?;

    while let Some(line) = read_bounded_line(&mut reader, MAX_LINE_BYTES).await? {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }

        tracing::trace!(bytes = line.len(), "wiri event received");

        if parse_event(&line)? && !enqueue_workspace(swap_tx) {
            // Receiver dropped — daemon is shutting down
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Event parsing
// ---------------------------------------------------------------------------

fn verify_subscription_ack(line: &[u8]) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(line)?;
    if value.get("success").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(());
    }
    bail!(
        "wiri subscription rejected: {}",
        value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("invalid acknowledgement")
    )
}

fn parse_event(line: &[u8]) -> Result<bool> {
    let value: serde_json::Value = serde_json::from_slice(line)?;
    Ok(matches!(
        value.get("type").and_then(serde_json::Value::as_str),
        Some("workspace_switched" | "WorkspaceSwitched")
    ))
}

fn enqueue_workspace(swap_tx: &mpsc::Sender<SwapRequest>) -> bool {
    match swap_tx.try_send(SwapRequest {
        reason: SwapReason::WorkspaceChange,
        specific: None,
    }) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::debug!("swap queue full; coalescing workspace event");
            true
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if consumed > max_bytes.saturating_sub(line.len()) {
            bail!("wiri IPC line exceeds {max_bytes} bytes");
        }
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);

        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

fn verify_server_session(pipe: &tokio::net::windows::named_pipe::NamedPipeClient) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::GetNamedPipeServerSessionId;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;

    let mut server_session = 0;
    unsafe {
        GetNamedPipeServerSessionId(HANDLE(pipe.as_raw_handle()), &mut server_session)
            .context("query wiri pipe server session")?;
    }
    let mut current_session = 0;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut current_session)
            .context("query current process session")?;
    }
    ensure_same_session(server_session, current_session)
}

fn ensure_same_session(server_session: u32, current_session: u32) -> Result<()> {
    if server_session != current_session {
        bail!(
            "wiri pipe belongs to Windows session {server_session}, current session is {current_session}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[test]
    fn validates_ack_and_parses_workspace_events() {
        assert!(verify_subscription_ack(br#"{"success":true,"subscription_id":1}"#).is_ok());
        assert!(verify_subscription_ack(br#"{"success":false,"error":"unsupported"}"#).is_err());
        assert!(parse_event(br#"{"type":"workspace_switched"}"#).unwrap());
        assert!(!parse_event(br#"{"type":"other"}"#).unwrap());
        assert!(parse_event(b"{").is_err());
    }

    #[tokio::test]
    async fn rejects_oversized_and_waits_for_complete_lines() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"123456789\n").await.unwrap();
        drop(writer);
        let error = read_bounded_line(&mut BufReader::new(reader), 8)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 8 bytes"));

        let (mut writer, reader) = tokio::io::duplex(32);
        let read = tokio::spawn(async move {
            read_bounded_line(&mut BufReader::new(reader), 8)
                .await
                .unwrap()
        });
        writer.write_all(b"1234").await.unwrap();
        tokio::task::yield_now().await;
        assert!(!read.is_finished());
        writer.write_all(b"567\n").await.unwrap();
        assert_eq!(read.await.unwrap().unwrap(), b"1234567\n");
    }

    #[test]
    fn rejects_cross_session_pipe_servers() {
        assert!(ensure_same_session(7, 7).is_ok());
        assert!(ensure_same_session(8, 7).is_err());
    }

    #[test]
    fn full_queue_coalesces_workspace_events() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(SwapRequest {
            reason: SwapReason::Interval,
            specific: None,
        })
        .unwrap();

        assert!(enqueue_workspace(&tx));
        assert!(matches!(
            rx.try_recv().unwrap().reason,
            SwapReason::Interval
        ));
        assert!(rx.try_recv().is_err());
    }
}

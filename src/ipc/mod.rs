pub mod messages;
pub mod server;

pub use messages::{IpcError, IpcEvent, IpcMessage};
pub use server::IpcServer;

/// Named pipe path used by both daemon and ctl.
pub const PIPE_PATH: &str = r"\\.\pipe\aurora";

/// Maximum JSON payload in one IPC frame.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Read one length-prefixed IPC frame. A clean EOF before the header is
/// returned as `Ok(None)`; partial/truncated frames are errors.
pub async fn read_frame<R>(reader: &mut R) -> anyhow::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut header = [0u8; 4];
    let mut offset = 0;
    while offset < header.len() {
        let n = reader.read(&mut header[offset..]).await?;
        if n == 0 {
            if offset == 0 {
                return Ok(None);
            }
            anyhow::bail!("truncated IPC frame header");
        }
        offset += n;
    }

    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("IPC frame exceeds {} byte limit", MAX_FRAME_SIZE);
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

/// Write one length-prefixed IPC frame.
pub async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    if payload.len() > MAX_FRAME_SIZE {
        anyhow::bail!("IPC frame exceeds {} byte limit", MAX_FRAME_SIZE);
    }
    let len = (payload.len() as u32).to_le_bytes();
    writer.write_all(&len).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Send a single `IpcMessage` to a running aurora daemon and return the
/// raw JSON response bytes.  Used by `aurora-ctl`.
pub async fn send_message(msg: &IpcMessage) -> anyhow::Result<Vec<u8>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut client = ClientOptions::new()
        .open(PIPE_PATH)
        .map_err(|e| anyhow::anyhow!("Cannot connect to aurora daemon at {}: {}", PIPE_PATH, e))?;

    let bytes = serde_json::to_vec(msg)?;
    write_frame(&mut client, &bytes).await?;
    read_frame(&mut client)
        .await?
        .ok_or_else(|| anyhow::anyhow!("aurora daemon closed IPC pipe without a response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn frame_roundtrip_handles_short_reads() {
        let (mut tx, mut rx) = tokio::io::duplex(64);
        let payload = br#"{"ok":true}"#.to_vec();
        let expected = payload.clone();
        let writer = tokio::spawn(async move {
            let header = (payload.len() as u32).to_le_bytes();
            tx.write_all(&header[..1]).await.unwrap();
            tx.write_all(&header[1..]).await.unwrap();
            tx.write_all(&payload).await.unwrap();
        });

        assert_eq!(read_frame(&mut rx).await.unwrap(), Some(expected));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn frame_limit_rejects_oversized_payload() {
        let (mut tx, mut rx) = tokio::io::duplex(64);
        tx.write_all(&((MAX_FRAME_SIZE as u32) + 1).to_le_bytes())
            .await
            .unwrap();
        drop(tx);
        assert!(read_frame(&mut rx).await.is_err());
    }
}

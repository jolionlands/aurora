pub mod messages;
pub mod server;

pub use messages::{IpcError, IpcEvent, IpcMessage};
pub use server::IpcServer;

/// Named pipe path used by both daemon and ctl.
pub const PIPE_PATH: &str = r"\\.\pipe\aurora";

/// Send a single `IpcMessage` to a running aurora daemon and return the
/// raw JSON response bytes.  Used by `aurora-ctl`.
pub async fn send_message(msg: &IpcMessage) -> anyhow::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut client = ClientOptions::new()
        .open(PIPE_PATH)
        .map_err(|e| anyhow::anyhow!("Cannot connect to aurora daemon at {}: {}", PIPE_PATH, e))?;

    let bytes = serde_json::to_vec(msg)?;
    client.write_all(&bytes).await?;
    client.flush().await?;

    let mut response = Vec::new();
    client.read_to_end(&mut response).await?;
    Ok(response)
}

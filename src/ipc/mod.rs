pub mod messages;
pub mod server;

pub use messages::{IpcEvent, IpcMessage};
pub use server::IpcServer;

fn current_session_id() -> anyhow::Result<u32> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;

    let mut session_id = 0;
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session_id)? };
    Ok(session_id)
}

/// Named pipe path used by the daemon and ctl in the current Windows session.
pub fn pipe_path() -> anyhow::Result<String> {
    Ok(pipe_path_for_session(current_session_id()?))
}

fn pipe_path_for_session(session_id: u32) -> String {
    format!(r"\\.\pipe\aurora-{session_id}")
}

/// Held for the daemon lifetime so a brief gap between pipe instances cannot
/// admit a second daemon in the same Windows session.
pub struct SingletonGuard(windows::Win32::Foundation::HANDLE);

impl SingletonGuard {
    pub fn acquire() -> anyhow::Result<Self> {
        acquire_named_singleton(&format!(r"Local\Aurora-{}", current_session_id()?))
    }
}

fn acquire_named_singleton(name: &str) -> anyhow::Result<SingletonGuard> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS,
    };
    use windows::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { SetLastError(ERROR_SUCCESS) };
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr()))? };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        anyhow::bail!("another Aurora instance is already running in this Windows session");
    }
    Ok(SingletonGuard(handle))
}

impl Drop for SingletonGuard {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;

        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Maximum JSON payload in one IPC frame.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Bound frame setup and writes so a peer cannot hold an IPC task forever.
pub const FRAME_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Source-changing commands may legitimately take several minutes.
pub const COMMAND_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn command_response_timeout(msg: &IpcMessage) -> std::time::Duration {
    match msg {
        IpcMessage::Reload | IpcMessage::SetFolder { .. } => COMMAND_RESPONSE_TIMEOUT,
        _ => FRAME_IO_TIMEOUT,
    }
}

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

/// Read a complete frame under one deadline, including its header and payload.
pub async fn read_frame_with_timeout<R>(
    reader: &mut R,
    timeout: std::time::Duration,
) -> anyhow::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    tokio::time::timeout(timeout, read_frame(reader))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out reading IPC frame after {} ms",
                timeout.as_millis()
            )
        })?
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
    writer.write_u32_le(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Write and flush a complete frame under one deadline.
pub async fn write_frame_with_timeout<W>(
    writer: &mut W,
    payload: &[u8],
    timeout: std::time::Duration,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    tokio::time::timeout(timeout, write_frame(writer, payload))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out writing IPC frame after {} ms",
                timeout.as_millis()
            )
        })?
}

/// Wait briefly for a free server instance when Windows reports a busy pipe.
pub async fn open_pipe_client(
    path: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    use windows::Win32::Foundation::ERROR_PIPE_BUSY;

    let connect = async {
        loop {
            match ClientOptions::new().open(path) {
                Ok(client) => return Ok(client),
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error),
            }
        }
    };

    tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Cannot connect to aurora daemon at {path}: IPC remained busy for {} ms",
                timeout.as_millis()
            )
        })?
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "Cannot connect to aurora daemon at {path}: aurora is not running in this Windows session ({error})"
                )
            } else {
                anyhow::anyhow!("Cannot connect to aurora daemon at {path}: {error}")
            }
        })
}

/// Send a single `IpcMessage` to a running aurora daemon and return the
/// raw JSON response bytes. Used by `aurora-ctl`.
pub async fn send_message(msg: &IpcMessage) -> anyhow::Result<Vec<u8>> {
    let path = pipe_path()?;
    let mut client = open_pipe_client(&path, FRAME_IO_TIMEOUT).await?;

    let bytes = serde_json::to_vec(msg)?;
    write_frame_with_timeout(&mut client, &bytes, FRAME_IO_TIMEOUT).await?;
    let response = read_frame_with_timeout(&mut client, command_response_timeout(msg))
        .await
        .map_err(|error| {
            error.context(format!(
                "aurora daemon at {path} accepted the IPC request but did not complete a response; it may still be starting"
            ))
        })?;
    response.ok_or_else(|| anyhow::anyhow!("aurora daemon closed IPC pipe without a response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn pipe_name_is_scoped_to_session() {
        assert_eq!(pipe_path_for_session(0), r"\\.\pipe\aurora-0");
        assert_eq!(pipe_path_for_session(42), r"\\.\pipe\aurora-42");
        assert_ne!(pipe_path_for_session(1), pipe_path_for_session(2));
    }

    #[test]
    fn singleton_rejects_a_second_owner() {
        let name = format!(r"Local\Aurora-test-{}", std::process::id());
        let _owner = acquire_named_singleton(&name).expect("acquire singleton");
        assert!(acquire_named_singleton(&name).is_err());
    }

    #[test]
    fn only_source_commands_use_the_long_response_timeout() {
        assert_eq!(
            command_response_timeout(&IpcMessage::Reload),
            std::time::Duration::from_secs(600)
        );
        assert_eq!(
            command_response_timeout(&IpcMessage::SetFolder {
                path: "C:\\wallpapers".to_string()
            }),
            std::time::Duration::from_secs(600)
        );
        assert_eq!(
            command_response_timeout(&IpcMessage::Status),
            FRAME_IO_TIMEOUT
        );
    }

    #[tokio::test]
    async fn pipe_client_retries_busy_instances_and_explains_failures() {
        use tokio::net::windows::named_pipe::ServerOptions;

        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing = format!(r"\\.\pipe\aurora-test-missing-{suffix}");
        let error = open_pipe_client(&missing, std::time::Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not running"));

        let path = format!(r"\\.\pipe\aurora-test-busy-{suffix}");
        let first_server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&path)
            .unwrap();
        let mut first_client = open_pipe_client(&path, std::time::Duration::from_millis(100))
            .await
            .unwrap();
        write_frame(&mut first_client, b"startup").await.unwrap();
        first_server.connect().await.unwrap();
        let mut first_server = first_server;
        assert_eq!(
            read_frame(&mut first_server).await.unwrap().unwrap(),
            b"startup"
        );
        write_frame(&mut first_server, b"ready").await.unwrap();
        assert_eq!(
            read_frame(&mut first_client).await.unwrap().unwrap(),
            b"ready"
        );

        let error = open_pipe_client(&path, std::time::Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("IPC remained busy"));

        let waiting = tokio::spawn({
            let path = path.clone();
            async move { open_pipe_client(&path, std::time::Duration::from_secs(1)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        assert!(!waiting.is_finished());

        let second_server = ServerOptions::new().create(&path).unwrap();
        let second_client = waiting.await.unwrap().unwrap();
        second_server.connect().await.unwrap();

        drop((first_client, first_server, second_client, second_server));
    }

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

    #[tokio::test]
    async fn frame_deadline_covers_stalled_header_and_write() {
        let (mut tx, mut rx) = tokio::io::duplex(8);
        tx.write_all(&[1]).await.unwrap();
        let read_error = read_frame_with_timeout(&mut rx, std::time::Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(read_error
            .to_string()
            .contains("timed out reading IPC frame"));

        let (mut tx, _rx) = tokio::io::duplex(1);
        let write_error =
            write_frame_with_timeout(&mut tx, &[0; 64], std::time::Duration::from_millis(10))
                .await
                .unwrap_err();
        assert!(write_error
            .to_string()
            .contains("timed out writing IPC frame"));
    }

    #[tokio::test]
    async fn timed_frame_read_reports_truncated_header_without_waiting_for_deadline() {
        let (mut tx, mut rx) = tokio::io::duplex(8);
        tx.write_all(&[1, 0]).await.unwrap();
        drop(tx);

        let error = read_frame_with_timeout(&mut rx, std::time::Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("truncated IPC frame header"));
    }
}

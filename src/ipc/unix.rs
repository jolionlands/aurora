//! User-private Unix sockets and a flock singleton with crash recovery.
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn runtime_directory() -> Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("aurora"))
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/aurora-{uid}")));
    match std::fs::create_dir(&directory) {
        Ok(()) => std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).context("create Aurora runtime directory"),
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        anyhow::bail!(
            "Aurora runtime directory must be a private directory owned by the current user: {}",
            directory.display()
        );
    }
    Ok(directory)
}

pub fn pipe_path() -> Result<String> {
    let path = runtime_directory()?.join("control.sock");
    let value = path
        .to_str()
        .context("Aurora runtime path is not valid UTF-8")?;
    if value.len() >= 108 {
        anyhow::bail!("Aurora Unix socket path is too long");
    }
    Ok(value.to_owned())
}

pub struct SingletonGuard(File);
impl SingletonGuard {
    pub fn acquire() -> Result<Self> {
        acquire(&runtime_directory()?.join("daemon.lock"))
    }
}

fn acquire(path: &Path) -> Result<SingletonGuard> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!("another Aurora instance is already running for this user");
    }
    Ok(SingletonGuard(file))
}

impl Drop for SingletonGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn bind_listener(path: &str) -> Result<tokio::net::UnixListener> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!(
                "refusing to replace a non-socket or another user's IPC endpoint: {path}"
            );
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!("another Aurora instance is already listening at {path}");
        }
        std::fs::remove_file(path).context("remove stale Aurora socket")?;
    }
    let listener = tokio::net::UnixListener::bind(path).context("bind Aurora Unix socket")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn open_pipe_client(path: &str, timeout: Duration) -> Result<tokio::net::UnixStream> {
    tokio::time::timeout(timeout, tokio::net::UnixStream::connect(path))
        .await
        .context("timed out connecting to Aurora")?
        .with_context(|| {
            format!("cannot connect to Aurora at {path}: daemon is not running or unavailable")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn singleton_rejects_a_second_owner_and_releases_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let owner = acquire(&path).unwrap();
        assert!(acquire(&path).is_err());
        drop(owner);
        assert!(acquire(&path).is_ok());
    }
    #[tokio::test]
    async fn socket_roundtrip_is_private_and_reclaims_stale_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let path = path.to_str().unwrap();
        let listener = bind_listener(path).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        assert!(bind_listener(path).is_err());
        // The failed second bind probes the live endpoint, so drain the probe.
        drop(listener.accept().await.unwrap());
        let mut client = open_pipe_client(path, Duration::from_secs(1))
            .await
            .unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        crate::ipc::write_frame(&mut client, b"request")
            .await
            .unwrap();
        assert_eq!(
            crate::ipc::read_frame(&mut server).await.unwrap().unwrap(),
            b"request"
        );
        crate::ipc::write_frame(&mut server, b"response")
            .await
            .unwrap();
        assert_eq!(
            crate::ipc::read_frame(&mut client).await.unwrap().unwrap(),
            b"response"
        );
        drop((client, server, listener));
        // Other tests spawn subprocesses concurrently. Between fork and exec,
        // a child can briefly retain the CLOEXEC listener descriptor. Allow
        // that interval to end before proving stale endpoint reclamation.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match bind_listener(path) {
                Ok(_) => break,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("reclaim stale socket: {error}"),
            }
        }
    }
    #[tokio::test]
    async fn regular_files_are_never_deleted_for_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        std::fs::write(&path, b"keep").unwrap();
        assert!(bind_listener(path.to_str().unwrap()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
    }
}

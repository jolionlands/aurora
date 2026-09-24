//! Log output for the daemon.
//!
//! Aurora is a Windows-subsystem program: started from the Run key or with
//! `Start-Process` it has no stderr, and every `warn!` would vanish. When
//! stderr is not usable, logs go to a size-capped file next to the config
//! (`aurora.log`, rotated once to `aurora.log.1`).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Rotate once the active log reaches this size.
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
pub const LOG_FILENAME: &str = "aurora.log";

/// True when this process has a real stderr (console or redirection).
pub fn stderr_is_usable() -> bool {
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
    unsafe { GetStdHandle(STD_ERROR_HANDLE) }
        .is_ok_and(|handle| !handle.is_invalid() && !handle.0.is_null())
}

/// Append-only log file that rotates to `<name>.1` at `max_bytes`.
pub struct RotatingFile {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
}

impl RotatingFile {
    pub fn open(path: &Path, max_bytes: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create log directory {}", parent.display()))?;
        }
        let file = open_append(path)?;
        let written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let mut log = Self {
            path: path.to_path_buf(),
            file,
            written,
            max_bytes,
        };
        if log.written >= max_bytes {
            log.rotate()?;
        }
        Ok(log)
    }

    fn rotated_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(".1");
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> Result<()> {
        self.file.flush().ok();
        // Replace the previous rotation; the handle stays valid while renamed.
        std::fs::rename(&self.path, self.rotated_path())
            .with_context(|| format!("rotate log {}", self.path.display()))?;
        self.file = open_append(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open log {}", path.display()))
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(buf.len() as u64) > self.max_bytes && self.written > 0 {
            // Keep logging to the current file if rotation fails.
            let _ = self.rotate();
        }
        let written = self.file.write(buf)?;
        self.written = self.written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_once_at_the_size_cap_and_keeps_one_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join(LOG_FILENAME);
        let mut log = RotatingFile::open(&path, 100).unwrap();
        log.write_all(&[b'a'; 80]).unwrap();
        log.write_all(&[b'b'; 40]).unwrap();
        log.write_all(&[b'c'; 70]).unwrap();
        log.write_all(&[b'd'; 40]).unwrap();
        log.flush().unwrap();

        let current = std::fs::read(&path).unwrap();
        let previous = std::fs::read(dir.path().join("logs").join("aurora.log.1")).unwrap();
        assert_eq!(current, [b'd'; 40]);
        assert_eq!(previous, [b'c'; 70]);
    }

    #[test]
    fn reopening_an_oversized_log_rotates_it_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILENAME);
        std::fs::write(&path, [b'x'; 150]).unwrap();
        let mut log = RotatingFile::open(&path, 100).unwrap();
        log.write_all(b"fresh").unwrap();
        log.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh");
        assert_eq!(
            std::fs::read(dir.path().join("aurora.log.1"))
                .unwrap()
                .len(),
            150
        );
    }
}

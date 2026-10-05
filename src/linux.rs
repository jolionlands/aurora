//! Linux paths and bounded subprocess execution.
use anyhow::{Context, Result};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Output};
use std::time::{Duration, Instant};

pub fn config_home() -> PathBuf {
    xdg_home("XDG_CONFIG_HOME", ".config")
}

pub fn data_home() -> PathBuf {
    xdg_home("XDG_DATA_HOME", ".local/share")
}

fn xdg_home(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(fallback)
        })
}

/// Drain both pipes while waiting so a full pipe cannot deadlock a helper.
/// Bound the retained output; continue draining after the cap is reached.
pub fn wait_for_child(mut child: Child, timeout: Duration) -> Result<Output> {
    const MAX_OUTPUT: usize = 1024 * 1024;
    fn drain(pipe: Option<impl Read + Send + 'static>) -> Option<std::thread::JoinHandle<Vec<u8>>> {
        pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut output = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let keep = n.min(MAX_OUTPUT.saturating_sub(output.len()));
                            output.extend_from_slice(&chunk[..keep]);
                        }
                    }
                }
                output
            })
        })
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let collect = |reader: Option<std::thread::JoinHandle<Vec<u8>>>| {
        reader
            .map(|r| r.join().unwrap_or_default())
            .unwrap_or_default()
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for wallpaper helper")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            collect(stdout);
            collect(stderr);
            anyhow::bail!(
                "wallpaper helper timed out after {} milliseconds",
                timeout.as_millis()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn helper_is_killed_and_reaped_at_timeout() {
        let child = Command::new("sleep").arg("5").spawn().unwrap();
        let start = Instant::now();
        let error = wait_for_child(child, Duration::from_millis(50)).unwrap_err();
        assert!(error
            .to_string()
            .contains("timed out after 50 milliseconds"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn full_pipes_are_drained_without_deadlock() {
        let child = Command::new("sh")
            .args([
                "-c",
                "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = wait_for_child(child, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 200000);
        assert_eq!(output.stderr.len(), 200000);
    }
}

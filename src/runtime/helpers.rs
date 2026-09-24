//! Short-lived `aurora.exe` helper processes that talk to the shell wallpaper API.

use super::*;

pub(super) const DIRECT_APPLY_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn apply_direct_in_child(
    path: &Path,
    fit: Option<&str>,
    monitor_id: Option<&str>,
) -> Result<()> {
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    let mut command = Command::new(std::env::current_exe().context("locate aurora executable")?);
    command.arg("--apply-once").arg(path);
    if let Some(fit) = fit {
        command.arg("--apply-fit").arg(fit);
    }
    if let Some(monitor_id) = monitor_id {
        command.arg("--apply-monitor").arg(monitor_id);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0);
    let child = command.spawn().context("start wallpaper apply helper")?;
    let output = wait_for_helper_child(child, DIRECT_APPLY_TIMEOUT)?;
    if !output.status.success() {
        anyhow::bail!(
            "wallpaper apply helper failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub(super) fn inspect_wallpapers_in_child() -> Result<Vec<MonitorSnapshot>> {
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    let mut command = Command::new(std::env::current_exe().context("locate aurora executable")?);
    command
        .arg("--inspect-wallpapers")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0);
    let child = command
        .spawn()
        .context("start wallpaper inspection helper")?;
    let output = wait_for_helper_child(child, DIRECT_APPLY_TIMEOUT)?;
    if !output.status.success() {
        anyhow::bail!(
            "wallpaper inspection helper failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("parse wallpaper inspection helper output")
}

/// Largest helper stdout/stderr Aurora keeps; helpers print a few KB at most.
pub(super) const MAX_HELPER_OUTPUT: u64 = 1024 * 1024;

/// Wait for a helper without polling, draining its pipes concurrently so a
/// chatty helper cannot block on a full pipe until the timeout kills it.
pub(super) fn wait_for_helper_child(mut child: Child, timeout: Duration) -> Result<Output> {
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::WaitForSingleObject;

    fn drain(pipe: Option<impl Read + Send + 'static>) -> Option<std::thread::JoinHandle<Vec<u8>>> {
        pipe.map(|pipe| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.take(MAX_HELPER_OUTPUT).read_to_end(&mut buffer);
                buffer
            })
        })
    }
    let collect = |reader: Option<std::thread::JoinHandle<Vec<u8>>>| {
        reader
            .map(|reader| reader.join().unwrap_or_default())
            .unwrap_or_default()
    };

    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    let waited = unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), millis) };

    if waited == WAIT_TIMEOUT {
        let kill_error = child.kill().err();
        // Reap the helper (closing its pipes) before joining the readers. A
        // failed kill is fine if the helper exited on its own meanwhile.
        let terminated = match kill_error {
            None => child.wait().is_ok(),
            Some(_) => child.try_wait().ok().flatten().is_some(),
        };
        if terminated {
            collect(stdout);
            collect(stderr);
        }
        // Otherwise the readers are detached: joining would block on pipes
        // the surviving helper still holds open.
        if let (Some(error), false) = (kill_error, terminated) {
            anyhow::bail!(
                "wallpaper helper timed out after {} milliseconds and could not be terminated: {error}",
                timeout.as_millis()
            );
        }
        anyhow::bail!(
            "wallpaper helper timed out after {} milliseconds",
            timeout.as_millis()
        );
    }
    if waited != WAIT_OBJECT_0 {
        let _ = child.kill();
        let _ = child.wait();
        collect(stdout);
        collect(stderr);
        anyhow::bail!("waiting for wallpaper helper failed: {waited:?}");
    }
    let status = child.wait().context("collect wallpaper helper status")?;
    Ok(Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

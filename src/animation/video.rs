//! Native streaming decoder embedded in the same desktop layer as GIFs.
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use super::desktop::{DesktopHost, PlayerWindow};
use super::DisplayTarget;
use crate::apply::WallpaperFit;
use crate::media::{mpv_executable, OwnedProcess};

pub(super) struct VideoPlayback {
    // Drop the child before its parent window.
    process: Option<OwnedProcess>,
    window: Option<PlayerWindow>,
    pipe: String,
    pub frozen: bool,
    pause_sent: Option<bool>,
}

impl VideoPlayback {
    pub fn pending(generation: u64) -> Self {
        Self {
            process: None,
            window: None,
            pipe: format!(r"\\.\pipe\aurora-video-{}-{generation}", std::process::id()),
            frozen: true,
            pause_sent: None,
        }
    }

    pub fn needs_window(&self) -> bool {
        self.process.is_none() && !self.frozen
    }

    pub fn reset_window(&mut self) {
        self.process = None;
        self.window = None;
        self.pause_sent = None;
    }

    pub fn update_policy(&mut self, frozen: bool) -> Result<()> {
        if self
            .window
            .as_ref()
            .is_some_and(|window| !window.is_alive())
        {
            self.reset_window();
        }
        if self
            .process
            .as_mut()
            .is_some_and(|process| !process.running())
        {
            anyhow::bail!("mpv exited; showing static video poster");
        }
        self.frozen = frozen;
        if self.process.is_some() && self.pause_sent != Some(frozen) {
            // A newly launched mpv may not have created its pipe yet. Retry on
            // the next policy check without blocking the desktop message loop.
            if let Ok(mut pipe) = std::fs::OpenOptions::new().write(true).open(&self.pipe) {
                let message = serde_json::json!({"command": ["set_property", "pause", frozen]});
                writeln!(pipe, "{message}").context("set video power/cover pause")?;
                self.pause_sent = Some(frozen);
            }
        }
        Ok(())
    }

    pub fn start(&mut self, host: &DesktopHost, target: &DisplayTarget, path: &Path) -> Result<()> {
        let window = PlayerWindow::create(host, target.bounds)?;
        let mut command = Command::new(mpv_executable());
        command.args(playback_arguments(target.fit));
        command.arg(format!("--wid={}", window.hwnd_id()));
        command.arg(format!("--input-ipc-server={}", self.pipe));
        command.arg("--").arg(path);
        let process = OwnedProcess::spawn(command)?;
        tracing::info!(path = %path.display(), pid = process.id(), pipe = %self.pipe,
            "playing streaming video wallpaper at source resolution and frame rate");
        self.window = Some(window);
        self.process = Some(process);
        self.pause_sent = None;
        Ok(())
    }
}

fn playback_arguments(fit: WallpaperFit) -> Vec<&'static str> {
    let mut arguments = vec![
        "--no-config",
        "--terminal=no",
        "--idle=no",
        "--keep-open=no",
        "--vo=gpu",
        "--gpu-context=d3d11",
        "--hwdec=auto",
        "--loop-file=inf",
        "--audio=no",
        "--osc=no",
        "--osd-level=0",
        "--input-media-keys=no",
        "--media-controls=no",
        "--input-vo-keyboard=no",
        "--input-cursor=no",
        "--load-scripts=no",
        "--autoload-files=no",
        "--stop-screensaver=no",
        "--cache=no",
        "--demuxer-max-bytes=8MiB",
        "--demuxer-max-back-bytes=0",
    ];
    match fit {
        WallpaperFit::Contain => arguments.push("--panscan=0"),
        WallpaperFit::Stretch => arguments.push("--keepaspect=no"),
        // Match the GIF backend for modes that one per-display movie cannot express.
        _ => arguments.push("--panscan=1"),
    }
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn video_fit_and_streaming_do_not_reduce_source_resolution_or_fps() {
        let fill = playback_arguments(WallpaperFit::Fill);
        assert!(fill.contains(&"--panscan=1"));
        assert!(playback_arguments(WallpaperFit::Contain).contains(&"--panscan=0"));
        assert!(playback_arguments(WallpaperFit::Stretch).contains(&"--keepaspect=no"));
        assert!(fill.contains(&"--cache=no"));
        assert!(!fill
            .iter()
            .any(|arg| arg.contains("fps=") || arg.contains("scale=")));
    }
}

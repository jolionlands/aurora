//! Native streaming decoder embedded in the same desktop layer as GIFs.
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::desktop::{DesktopHost, PlayerWindow};
use super::DisplayTarget;
use crate::apply::WallpaperFit;
use crate::config::types::AnimationScaling;
use crate::media::{mpv_executable, OwnedProcess};

/// mpv's own window appears inside ours once its video output exists; give it
/// a moment to present before showing ours, so the poster never flashes black.
const REVEAL_AFTER_CHILD: Duration = Duration::from_millis(100);
/// Show the player anyway if mpv never reports a window.
const REVEAL_FALLBACK: Duration = Duration::from_secs(2);
/// How often to look for mpv's window while ours is still hidden.
const REVEAL_POLL: Duration = Duration::from_millis(25);

pub(super) struct VideoPlayback {
    // Drop the child before its parent window.
    process: Option<OwnedProcess>,
    window: Option<PlayerWindow>,
    pipe: String,
    pub frozen: bool,
    pause_sent: Option<bool>,
    /// When mpv started behind a still-hidden window; `None` once shown.
    started: Option<Instant>,
    /// When mpv's own window first appeared inside ours.
    child_seen: Option<Instant>,
}

impl VideoPlayback {
    pub fn pending(generation: u64) -> Self {
        Self {
            process: None,
            window: None,
            pipe: format!(r"\\.\pipe\aurora-video-{}-{generation}", std::process::id()),
            frozen: true,
            pause_sent: None,
            started: None,
            child_seen: None,
        }
    }

    pub fn needs_window(&self) -> bool {
        self.process.is_none() && !self.frozen
    }

    pub fn reset_window(&mut self) {
        self.process = None;
        self.window = None;
        self.pause_sent = None;
        self.started = None;
        self.child_seen = None;
    }

    /// Stop mpv while nobody can see it; the static poster shows instead and
    /// playback starts again once the display is visible.
    pub fn release_player(&mut self) -> bool {
        let had_player = self.process.is_some();
        self.reset_window();
        had_player
    }

    /// Show the hidden window once mpv has drawn into it. Returns when to
    /// look again while it is still hidden.
    pub fn poll_reveal(&mut self, now: Instant) -> Option<Instant> {
        let started = self.started?;
        let window = self.window.as_ref()?;
        if self.child_seen.is_none() && window.has_child() {
            self.child_seen = Some(now);
        }
        let due = reveal_due(started, self.child_seen);
        if now < due {
            return Some(due.min(now + REVEAL_POLL));
        }
        window.reveal();
        self.started = None;
        None
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

    pub fn start(
        &mut self,
        host: &DesktopHost,
        target: &DisplayTarget,
        path: &Path,
        scaling: AnimationScaling,
    ) -> Result<()> {
        // Hidden until mpv has drawn: the static poster shows meanwhile.
        let window = PlayerWindow::create_hidden(host, target.bounds)?;
        let mut command = Command::new(mpv_executable());
        command.args(playback_arguments(target.fit, scaling));
        command.arg(format!("--wid={}", window.hwnd_id()));
        command.arg(format!("--input-ipc-server={}", self.pipe));
        command.arg("--").arg(path);
        let process = OwnedProcess::spawn(command)?;
        tracing::info!(path = %path.display(), pid = process.id(), pipe = %self.pipe,
            "playing streaming video wallpaper at source resolution and frame rate");
        self.window = Some(window);
        self.process = Some(process);
        self.pause_sent = None;
        self.started = Some(Instant::now());
        self.child_seen = None;
        Ok(())
    }
}

/// When a hidden player window should be shown.
fn reveal_due(started: Instant, child_seen: Option<Instant>) -> Instant {
    let fallback = started + REVEAL_FALLBACK;
    child_seen.map_or(fallback, |seen| (seen + REVEAL_AFTER_CHILD).min(fallback))
}

fn playback_arguments(fit: WallpaperFit, scaling: AnimationScaling) -> Vec<&'static str> {
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
        // One bilinear pass into the swap chain. mpv's default convolution
        // scalers add full-screen intermediate passes: on a 1080p60 clip
        // filling a 2560x1600 panel they cost about five times the GPU time
        // and 80 MiB more memory.
        "--profile=fast",
        "--fbo-format=rgba8",
    ];
    if scaling == AnimationScaling::Nearest {
        // Crisp pixel art at fractional scales, still without convolution.
        arguments.push("--scale=oversample");
    }
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
        let fill = playback_arguments(WallpaperFit::Fill, AnimationScaling::Auto);
        assert!(fill.contains(&"--panscan=1"));
        assert!(
            playback_arguments(WallpaperFit::Contain, AnimationScaling::Auto)
                .contains(&"--panscan=0")
        );
        assert!(
            playback_arguments(WallpaperFit::Stretch, AnimationScaling::Auto)
                .contains(&"--keepaspect=no")
        );
        assert!(fill.contains(&"--cache=no"));
    }

    #[test]
    fn streaming_renders_in_one_cheap_pass_at_source_resolution_and_fps() {
        for scaling in [
            AnimationScaling::Auto,
            AnimationScaling::Smooth,
            AnimationScaling::Nearest,
        ] {
            let arguments = playback_arguments(WallpaperFit::Fill, scaling);
            let fast = arguments
                .iter()
                .position(|arg| *arg == "--profile=fast")
                .expect("single-pass renderer");
            assert!(arguments.contains(&"--fbo-format=rgba8"));
            assert!(
                !arguments
                    .iter()
                    .any(|arg| arg.contains("fps=") || arg.starts_with("--vf")),
                "frame rate and resolution stay the source's"
            );
            let scale = arguments.iter().position(|arg| arg.starts_with("--scale="));
            if scaling == AnimationScaling::Nearest {
                let scale = scale.expect("pixel-art scaler");
                assert_eq!(arguments[scale], "--scale=oversample");
                assert!(scale > fast, "must override the profile's bilinear");
            } else {
                assert_eq!(scale, None, "the profile's bilinear scaling");
            }
        }
    }

    #[test]
    fn releasing_a_covered_player_stops_mpv_until_visible_again() {
        let mut video = VideoPlayback::pending(1);
        assert!(!video.release_player(), "nothing to stop before mpv starts");
        let mut stand_in = Command::new("ping");
        stand_in.args(["-n", "30", "127.0.0.1"]);
        video.process = Some(OwnedProcess::spawn(stand_in).unwrap());
        video.started = Some(Instant::now());
        video.update_policy(true).unwrap();
        assert!(!video.needs_window());
        assert!(video.release_player());
        assert!(video.process.is_none() && video.window.is_none() && video.started.is_none());
        assert!(!video.needs_window(), "stays stopped while covered");
        video.update_policy(false).unwrap();
        assert!(video.needs_window(), "starts again once visible");
    }

    #[test]
    fn hidden_player_is_shown_soon_after_mpv_draws_or_by_the_fallback() {
        let start = Instant::now();
        assert_eq!(reveal_due(start, None), start + REVEAL_FALLBACK);
        let seen = start + Duration::from_millis(300);
        assert_eq!(reveal_due(start, Some(seen)), seen + REVEAL_AFTER_CHILD);
        let late = start + Duration::from_secs(5);
        assert_eq!(
            reveal_due(start, Some(late)),
            start + REVEAL_FALLBACK,
            "never later than the fallback"
        );
    }
}

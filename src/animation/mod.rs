//! Opt-in animated wallpapers (GIF, animated WebP and native streaming MP4/WebM).
//!
//! The normal static commit through `IDesktopWallpaper` always happens first,
//! so Windows keeps showing the file's first frame whenever animation is off,
//! frozen, or unavailable. When `animated { enabled true }` is configured, a
//! single player thread hosts playback in a desktop-layer window per display
//! (see [`desktop`]). GIF/WebP frames use Direct3D; videos stream through mpv.
//!
//! Cost model:
//! * disabled: no thread, no windows, no decoding;
//! * enabled with only static wallpapers: no thread;
//! * GIF/WebP playing: one GPU present per frame (at most `max-fps`), one policy
//!   check per second, and at most `max-memory-mb` of frames per display;
//! * video playing: source-resolution streaming at source FPS, one bilinear
//!   (or oversample) pass per frame, bounded demuxer buffers, and one policy
//!   check per second;
//! * frozen (fullscreen/maximized window, battery, battery saver, lock screen,
//!   display off): one policy check per second and no drawing; a video's
//!   player exits after 30 s covered or 60 s frozen everywhere.

pub mod desktop;
pub mod frames;
pub mod policy;
mod render;
mod video;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};

use crate::apply::WallpaperFit;
use crate::config::types::AnimatedConfig;
use crate::transition::Rect;
use desktop::{DesktopHost, PlayerWindow};
use frames::{Animation, FrameBudget};
use render::{Gpu, PresentError};

pub use frames::may_be_animated;

/// Policy check interval while something is visibly animating.
const POLICY_INTERVAL: Duration = Duration::from_secs(1);
/// While every animated display is hidden behind a window: how quickly
/// playback resumes once the window goes away.
const COVERED_POLICY_INTERVAL: Duration = Duration::from_secs(2);
/// While battery, lock, or display-off freezes everything. Power and session
/// notifications wake the player immediately; this is only a safety net.
const FROZEN_POLICY_INTERVAL: Duration = Duration::from_secs(60);
/// After this long frozen everywhere, decoded frames and GPU objects are
/// released; they are decoded again when playback may resume.
const RELEASE_FROZEN_AFTER: Duration = Duration::from_secs(60);
/// Covered this long, a video's player exits and the static poster shows
/// through; it starts again when the display is visible.
const RELEASE_COVERED_VIDEO_AFTER: Duration = Duration::from_secs(30);
/// If playback falls this far behind (e.g. after sleep), resynchronise
/// instead of fast-forwarding through the missed frames.
const MAX_FRAME_LAG: Duration = Duration::from_secs(1);

/// One display's animation request.
#[derive(Debug, Clone)]
pub struct DisplayTarget {
    pub monitor_id: String,
    pub bounds: Rect,
    pub fit: WallpaperFit,
}

enum Command {
    Show(DisplayTarget, PathBuf),
    Hide(String),
    HideAll,
    Decoded {
        monitor_id: String,
        generation: u64,
        result: Result<Option<Animation>>,
    },
    Shutdown,
}

/// Auto-reset event that wakes the player thread; safe to signal anywhere.
struct WakeEvent(HANDLE);

// SAFETY: an event handle may be signalled and closed from any thread.
unsafe impl Send for WakeEvent {}
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    fn new() -> Result<Self> {
        let handle = unsafe {
            windows::Win32::System::Threading::CreateEventW(None, false, false, None)
                .context("create animation wake event")?
        };
        Ok(Self(handle))
    }

    fn signal(&self) {
        let _ = unsafe { windows::Win32::System::Threading::SetEvent(self.0) };
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct PlayerThread {
    commands: mpsc::Sender<Command>,
    wake: Arc<WakeEvent>,
    join: Option<JoinHandle<()>>,
}

impl PlayerThread {
    fn send(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            self.wake.signal();
        }
    }
}

/// Handle owned by the runtime. The player thread starts on the first
/// animated wallpaper and stops when the handle is dropped.
pub struct AnimationPlayer {
    config: AnimatedConfig,
    thread: Option<PlayerThread>,
}

impl AnimationPlayer {
    /// `None` when animation is disabled, so the rest of Aurora pays nothing.
    pub fn from_config(config: &AnimatedConfig) -> Option<Self> {
        config.enabled.then(|| Self {
            config: config.clone(),
            thread: None,
        })
    }

    /// Play `path` on `target` if it is an animated GIF/WebP or video; otherwise make
    /// sure nothing covers that display's static wallpaper.
    pub fn show(&mut self, target: DisplayTarget, path: PathBuf) {
        if !may_be_animated(&path) && !crate::media::is_video(&path) {
            self.hide(&target.monitor_id);
            return;
        }
        match self.ensure_thread() {
            Ok(thread) => thread.send(Command::Show(target, path)),
            Err(error) => warn!("animated wallpaper unavailable: {error:#}"),
        }
    }

    pub fn hide(&mut self, monitor_id: &str) {
        if let Some(thread) = &self.thread {
            thread.send(Command::Hide(monitor_id.to_string()));
        }
    }

    pub fn hide_all(&mut self) {
        if let Some(thread) = &self.thread {
            thread.send(Command::HideAll);
        }
    }

    fn ensure_thread(&mut self) -> Result<&PlayerThread> {
        if self.thread.is_none() {
            let (commands, receiver) = mpsc::channel();
            let wake = Arc::new(WakeEvent::new()?);
            let config = self.config.clone();
            let thread_wake = Arc::clone(&wake);
            let thread_commands = commands.clone();
            let join = std::thread::Builder::new()
                .name("aurora-animation".into())
                .stack_size(256 * 1024)
                .spawn(move || {
                    PlayerLoop::new(config, receiver, thread_commands, thread_wake).run()
                })
                .context("start animation thread")?;
            self.thread = Some(PlayerThread {
                commands,
                wake,
                join: Some(join),
            });
        }
        Ok(self.thread.as_ref().expect("thread was just started"))
    }
}

impl Drop for AnimationPlayer {
    fn drop(&mut self) {
        if let Some(mut thread) = self.thread.take() {
            thread.send(Command::Shutdown);
            if let Some(join) = thread.join.take() {
                let _ = join.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Player thread
// ---------------------------------------------------------------------------

struct Playing {
    animation: Animation,
    /// CPU pixel copies were freed after every frame reached the GPU.
    pixels_released: bool,
    window: Option<PlayerWindow>,
    frame: usize,
    next_due: Instant,
    frozen: bool,
    /// Suppresses repeated warnings while presenting keeps failing.
    draw_failed: bool,
}

struct Display {
    target: DisplayTarget,
    path: PathBuf,
    generation: u64,
    playing: Option<Playing>,
    video: Option<video::VideoPlayback>,
    /// Frames were released during a long freeze; decode again on resume.
    released: bool,
    /// When a maximized or fullscreen window started covering this display.
    covered_since: Option<Instant>,
}

/// Track how long a display has been covered; true once its player may go.
fn covered_long_enough(since: &mut Option<Instant>, covered: bool, now: Instant) -> bool {
    if !covered {
        *since = None;
        return false;
    }
    now.duration_since(*since.get_or_insert(now)) >= RELEASE_COVERED_VIDEO_AFTER
}

struct PlayerLoop {
    config: AnimatedConfig,
    commands: mpsc::Receiver<Command>,
    /// Used by decode workers to report results back to this thread.
    results: mpsc::Sender<Command>,
    wake: Arc<WakeEvent>,
    displays: HashMap<String, Display>,
    host: Option<DesktopHost>,
    next_generation: u64,
    next_policy: Instant,
    frozen_everywhere: bool,
    /// When the current everywhere-freeze began.
    frozen_since: Option<Instant>,
    power: Option<power::PowerWatch>,
    /// Direct3D/Direct2D objects; present only while something animates.
    gpu: Option<Gpu>,
    /// Next check for a hidden video window whose player may have drawn.
    reveal_due: Option<Instant>,
}

impl PlayerLoop {
    fn new(
        config: AnimatedConfig,
        commands: mpsc::Receiver<Command>,
        results: mpsc::Sender<Command>,
        wake: Arc<WakeEvent>,
    ) -> Self {
        Self {
            config,
            commands,
            results,
            wake,
            displays: HashMap::new(),
            host: None,
            next_generation: 0,
            next_policy: Instant::now(),
            frozen_everywhere: false,
            frozen_since: None,
            power: None,
            gpu: None,
            reveal_due: None,
        }
    }

    fn run(mut self) {
        self.power = power::PowerWatch::register()
            .inspect_err(|error| debug!("power notifications unavailable: {error:#}"))
            .ok();
        loop {
            while let Ok(command) = self.commands.try_recv() {
                if !self.handle(command) {
                    return;
                }
            }
            if !self.any_playing() {
                // Nothing animates: free the GPU device and its driver memory.
                self.gpu = None;
            }
            let now = Instant::now();
            if self.any_active() && now >= self.next_policy {
                self.apply_policy(now);
                let interval = if self.frozen_everywhere {
                    FROZEN_POLICY_INTERVAL
                } else if self.any_animating() {
                    POLICY_INTERVAL
                } else {
                    COVERED_POLICY_INTERVAL
                };
                self.next_policy = now + interval;
            }
            self.advance_frames(Instant::now());
            self.reveal_due = self.poll_reveals(Instant::now());
            let timeout = self.timeout(Instant::now());
            wait_and_pump(&self.wake, timeout);
            if power::take_change() {
                // Power source, battery saver, display, or lock changed.
                self.next_policy = Instant::now();
            }
        }
    }

    /// Returns false on shutdown.
    fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Show(target, path) => {
                let monitor_id = target.monitor_id.clone();
                // Replacing a display drops its old window right away; the
                // new file's first frame is already the static wallpaper.
                self.displays.insert(
                    monitor_id.clone(),
                    Display {
                        target,
                        path,
                        generation: 0,
                        playing: None,
                        video: None,
                        released: false,
                        covered_since: None,
                    },
                );
                self.start_decode(&monitor_id);
            }
            Command::Hide(monitor_id) => {
                self.displays.remove(&monitor_id);
            }
            Command::HideAll => self.displays.clear(),
            Command::Decoded {
                monitor_id,
                generation,
                result,
            } => self.install(&monitor_id, generation, result),
            Command::Shutdown => {
                self.displays.clear();
                return false;
            }
        }
        true
    }

    /// Decode a display's file off this thread: it pumps messages for windows
    /// parented to Explorer and must never stall.
    fn start_decode(&mut self, monitor_id: &str) {
        let Some(display) = self.displays.get_mut(monitor_id) else {
            return;
        };
        self.next_generation += 1;
        let generation = self.next_generation;
        display.generation = generation;
        display.released = false;
        if crate::media::is_video(&display.path) {
            display.video = Some(video::VideoPlayback::pending(generation));
            self.next_policy = Instant::now();
            return;
        }
        let budget = FrameBudget {
            display_width: display.target.bounds.width,
            display_height: display.target.bounds.height,
            max_bytes: (self.config.max_memory_mb as usize) * 1024 * 1024,
            max_frames: self.config.max_frames as usize,
            min_delay: Duration::from_millis(1000 / u64::from(self.config.max_fps.max(1))),
            scaling: self.config.scaling,
        };
        let path = display.path.clone();
        let monitor_id = monitor_id.to_string();
        let results = self.results.clone();
        let wake = Arc::clone(&self.wake);
        let spawned = std::thread::Builder::new()
            .name("aurora-animation-decode".into())
            .spawn(move || {
                let result = frames::decode_animation(&path, &budget);
                let _ = results.send(Command::Decoded {
                    monitor_id,
                    generation,
                    result,
                });
                wake.signal();
            });
        if let Err(error) = spawned {
            warn!("could not start animation decode: {error}");
        }
    }

    fn install(&mut self, monitor_id: &str, generation: u64, result: Result<Option<Animation>>) {
        let Some(entry) = self.displays.get_mut(monitor_id) else {
            return;
        };
        if entry.generation != generation {
            return; // superseded by a newer wallpaper
        }
        match result {
            Ok(Some(animation)) => {
                info!(
                    path = %entry.path.display(),
                    monitor = monitor_id,
                    frames = animation.frames.len(),
                    size = %format!("{}x{}", animation.width, animation.height),
                    mb = animation.bytes() / (1024 * 1024),
                    "playing animated wallpaper"
                );
                entry.playing = Some(Playing {
                    animation,
                    pixels_released: false,
                    window: None,
                    frame: 0,
                    next_due: Instant::now(),
                    frozen: false,
                    draw_failed: false,
                });
                // Evaluate power/cover policy before the first frame.
                self.next_policy = Instant::now();
            }
            Ok(None) => {
                debug!(path = %entry.path.display(), "wallpaper has one frame; nothing to animate");
                self.displays.remove(monitor_id);
            }
            Err(error) => {
                warn!(path = %entry.path.display(), "cannot animate wallpaper: {error:#}");
                self.displays.remove(monitor_id);
            }
        }
    }

    fn any_playing(&self) -> bool {
        self.displays
            .values()
            .any(|display| display.playing.is_some())
    }

    /// Some display is visibly animating (not frozen).
    fn any_animating(&self) -> bool {
        self.displays.values().any(|display| {
            display
                .playing
                .as_ref()
                .is_some_and(|playing| !playing.frozen)
                || display.video.as_ref().is_some_and(|video| !video.frozen)
        })
    }

    /// Something is playing or waiting to be decoded again after a freeze.
    fn any_active(&self) -> bool {
        self.displays
            .values()
            .any(|display| display.playing.is_some() || display.video.is_some() || display.released)
    }

    /// Release or restore frames across long everywhere-freezes.
    fn manage_long_freeze(&mut self, frozen_everywhere: bool, now: Instant) {
        if !frozen_everywhere {
            self.frozen_since = None;
            let released: Vec<String> = self
                .displays
                .iter()
                .filter(|(_, display)| display.released)
                .map(|(id, _)| id.clone())
                .collect();
            for monitor_id in released {
                debug!(monitor = %monitor_id, "resuming animated wallpaper after freeze");
                self.start_decode(&monitor_id);
            }
            return;
        }
        let since = *self.frozen_since.get_or_insert(now);
        if now.duration_since(since) < RELEASE_FROZEN_AFTER {
            return;
        }
        for entry in self.displays.values_mut() {
            let had_frames = entry.playing.take().is_some();
            let had_video = entry.video.take().is_some();
            if had_frames || had_video {
                entry.released = true;
                debug!(path = %entry.path.display(), "released frozen animation frames");
            }
        }
    }

    fn apply_policy(&mut self, now: Instant) {
        let display_off = self.power.as_ref().is_some_and(|power| power.display_off());
        let state = policy::sample_system_state(display_off);
        let frozen_everywhere = policy::pause_everywhere(&self.config, state);
        if frozen_everywhere != self.frozen_everywhere {
            debug!(
                ?state,
                frozen = frozen_everywhere,
                "animated wallpaper power policy changed"
            );
            self.frozen_everywhere = frozen_everywhere;
        }
        self.manage_long_freeze(frozen_everywhere, now);
        let covered = if self.config.pause_when_covered && !frozen_everywhere {
            policy::covered_monitors()
        } else {
            Vec::new()
        };

        // Explorer restarts destroy the shell windows and ours with them.
        if self.host.as_ref().is_some_and(|host| !host.is_alive()) {
            info!("desktop host changed; re-attaching animated wallpapers");
            self.host = None;
            for display in self.displays.values_mut() {
                if let Some(video) = &mut display.video {
                    video.reset_window();
                }
                if let Some(playing) = &mut display.playing {
                    playing.window = None;
                }
            }
        }

        let mut failed_videos = Vec::new();
        for (monitor_id, display) in &mut self.displays {
            let covered_here = covered
                .iter()
                .any(|rect| policy::same_rect(rect, &display.target.bounds));
            let frozen = frozen_everywhere || covered_here;
            let release = covered_long_enough(&mut display.covered_since, covered_here, now);
            if let Some(video) = &mut display.video {
                if let Err(error) = video.update_policy(frozen) {
                    let path = &display.path;
                    warn!(path = %path.display(), "video wallpaper failed: {error:#}");
                    failed_videos.push(monitor_id.clone());
                } else if release && video.release_player() {
                    let path = &display.path;
                    info!(path = %path.display(), "video wallpaper covered; stopped its player until visible");
                }
            }
            let Some(playing) = &mut display.playing else {
                continue;
            };
            if playing
                .window
                .as_ref()
                .is_some_and(|window| !window.is_alive())
            {
                playing.window = None;
            }
            if playing.frozen && !frozen {
                playing.next_due = now; // resume promptly, from the current frame
            }
            playing.frozen = frozen;
        }
        for monitor_id in failed_videos {
            self.displays.remove(&monitor_id);
        }

        // Create windows lazily, and never while frozen, so a wallpaper that
        // starts under a fullscreen game costs nothing until it is visible.
        let needs_window = self.displays.values().any(|display| {
            display
                .playing
                .as_ref()
                .is_some_and(|playing| !playing.frozen && playing.window.is_none())
        });
        let needs_video = self.displays.values().any(|display| {
            display
                .video
                .as_ref()
                .is_some_and(video::VideoPlayback::needs_window)
        });
        if (needs_window || needs_video) && self.host.is_none() {
            match DesktopHost::find() {
                Ok(host) => {
                    debug!(
                        raised = host.is_raised(),
                        "attached to desktop wallpaper layer"
                    );
                    self.host = Some(host);
                }
                Err(error) => warn!("cannot attach animated wallpaper to the desktop: {error:#}"),
            }
        }
        let Some(host) = self.host else {
            return;
        };
        let mut failed_videos = Vec::new();
        let scaling = self.config.scaling;
        for (monitor_id, display) in &mut self.displays {
            if let Some(video) = &mut display.video {
                if video.needs_window() {
                    if let Err(error) = video.start(&host, &display.target, &display.path, scaling)
                    {
                        let path = &display.path;
                        warn!(path = %path.display(), "cannot start video wallpaper: {error:#}");
                        failed_videos.push(monitor_id.clone());
                    }
                }
            }
        }
        for monitor_id in failed_videos {
            self.displays.remove(&monitor_id);
        }
        // Existing GIF windows also need a device rebuilt after a lost GPU.
        // Streaming players own their GPU; a video-only loop needs none here.
        if !self.any_playing() {
            return;
        }
        if self.gpu.is_none() {
            match Gpu::new() {
                Ok(gpu) => self.gpu = Some(gpu),
                Err(error) => {
                    warn!("animated wallpaper needs Direct3D 11: {error:#}");
                    return;
                }
            }
        }
        let mut failed = Vec::new();
        let mut outcomes = Vec::new();
        for (monitor_id, display) in &mut self.displays {
            let Some(playing) = &mut display.playing else {
                continue;
            };
            if playing.frozen || playing.window.is_some() {
                continue;
            }
            match PlayerWindow::create(&host, display.target.bounds) {
                Ok(window) => {
                    playing.window = Some(window);
                    if let Some(gpu) = &self.gpu {
                        outcomes.push((monitor_id.clone(), draw(gpu, playing, display.target.fit)));
                    }
                    playing.next_due = now + playing.animation.frames[playing.frame].delay;
                }
                Err(error) => {
                    warn!(monitor = %monitor_id, "cannot create animated wallpaper window: {error:#}");
                    failed.push(monitor_id.clone());
                }
            }
        }
        for monitor_id in failed {
            self.displays.remove(&monitor_id);
        }
        self.handle_outcomes(outcomes);
    }

    /// Recover from failed presents: rebuild the GPU after a device loss and
    /// decode again where the pixels were already released.
    fn handle_outcomes(&mut self, outcomes: Vec<(String, DrawOutcome)>) {
        if outcomes
            .iter()
            .any(|(_, outcome)| *outcome != DrawOutcome::Shown)
        {
            self.reset_gpu();
        }
        for (monitor_id, outcome) in outcomes {
            if outcome == DrawOutcome::NeedsDecode {
                if let Some(display) = self.displays.get_mut(&monitor_id) {
                    // The static first frame shows until the decode lands.
                    display.playing = None;
                }
                self.start_decode(&monitor_id);
            }
        }
    }

    /// Drop every GPU object after a device loss; the next frame rebuilds.
    fn reset_gpu(&mut self) {
        for playing in self
            .displays
            .values_mut()
            .filter_map(|d| d.playing.as_mut())
        {
            if let Some(window) = &mut playing.window {
                window.release_surface();
            }
        }
        self.gpu = None;
    }

    fn advance_frames(&mut self, now: Instant) {
        let repaint = desktop::take_repaint_request();
        let Some(gpu) = &self.gpu else {
            return;
        };
        let mut outcomes = Vec::new();
        for (monitor_id, display) in &mut self.displays {
            let Some(playing) = &mut display.playing else {
                continue;
            };
            if playing.window.is_none() {
                continue;
            }
            if playing.frozen || now < playing.next_due {
                if repaint {
                    outcomes.push((monitor_id.clone(), draw(gpu, playing, display.target.fit)));
                }
                continue;
            }
            let count = playing.animation.frames.len();
            playing.frame = (playing.frame + 1) % count;
            outcomes.push((monitor_id.clone(), draw(gpu, playing, display.target.fit)));
            let delay = playing.animation.frames[playing.frame].delay;
            playing.next_due = if now.duration_since(playing.next_due) > MAX_FRAME_LAG {
                now + delay
            } else {
                playing.next_due + delay
            };
        }
        outcomes.retain(|(_, outcome)| *outcome != DrawOutcome::Shown);
        self.handle_outcomes(outcomes);
    }

    /// Show hidden video windows whose player has drawn; returns when to look
    /// again for those still hidden.
    fn poll_reveals(&mut self, now: Instant) -> Option<Instant> {
        self.displays
            .values_mut()
            .filter_map(|display| display.video.as_mut()?.poll_reveal(now))
            .min()
    }

    /// How long the thread may sleep: forever when nothing plays.
    fn timeout(&self, now: Instant) -> Option<Duration> {
        let mut deadline: Option<Instant> = self
            .displays
            .values()
            .any(|display| display.released || display.video.is_some())
            .then_some(self.next_policy);
        if let Some(reveal) = self.reveal_due {
            deadline = Some(deadline.map_or(reveal, |current| current.min(reveal)));
        }
        for playing in self
            .displays
            .values()
            .filter_map(|display| display.playing.as_ref())
        {
            let due = if playing.frozen || playing.window.is_none() {
                self.next_policy
            } else {
                playing.next_due.min(self.next_policy)
            };
            deadline = Some(deadline.map_or(due, |current| current.min(due)));
        }
        deadline.map(|deadline| deadline.saturating_duration_since(now))
    }
}

/// What drawing a frame requires of the player loop.
#[derive(Debug, PartialEq, Eq)]
enum DrawOutcome {
    Shown,
    /// Recreate every GPU object (device loss).
    DeviceLost,
    /// The pixels are gone and the GPU copy too: decode the file again.
    NeedsDecode,
}

/// Present the current frame, then free CPU pixels once all frames are on
/// the GPU.
fn draw(gpu: &Gpu, playing: &mut Playing, fit: WallpaperFit) -> DrawOutcome {
    let Some(window) = &mut playing.window else {
        return DrawOutcome::Shown;
    };
    match window.show_frame(gpu, &playing.animation, playing.frame, fit) {
        Ok(()) => {
            playing.draw_failed = false;
            if !playing.pixels_released && window.frames_resident() {
                for frame in &mut playing.animation.frames {
                    frame.bgra = Box::default();
                }
                playing.pixels_released = true;
            }
            DrawOutcome::Shown
        }
        Err(PresentError::NotResident) => DrawOutcome::NeedsDecode,
        Err(PresentError::Device(error)) => {
            if !playing.draw_failed {
                warn!("animated wallpaper frame failed; recreating GPU resources: {error:#}");
            }
            playing.draw_failed = true;
            if playing.pixels_released {
                DrawOutcome::NeedsDecode
            } else {
                DrawOutcome::DeviceLost
            }
        }
    }
}

/// Sleep until `wake` is signalled, a window message arrives, or `timeout`
/// passes; then dispatch every queued message.
fn wait_and_pump(wake: &WakeEvent, timeout: Option<Duration>) {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG,
        MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT,
    };

    use windows::Win32::System::Threading::INFINITE;

    let millis = timeout.map_or(INFINITE, |timeout| {
        // Round up so a 0.4 ms remainder does not become a busy loop.
        u32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(INFINITE - 1)
    });
    unsafe {
        MsgWaitForMultipleObjectsEx(Some(&[wake.0]), millis, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        let mut message = MSG::default();
        while PeekMessageW(&mut message, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

// ---------------------------------------------------------------------------
// Display power notifications
// ---------------------------------------------------------------------------

mod power {
    //! Event-driven power, display and session state, so a frozen player can
    //! sleep instead of polling.

    use std::cell::Cell;

    use anyhow::{Context, Result};
    use windows::core::{w, GUID, PCWSTR};
    use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND};
    use windows::Win32::System::Power::{
        RegisterPowerSettingNotification, UnregisterPowerSettingNotification, HPOWERNOTIFY,
        POWERBROADCAST_SETTING,
    };
    use windows::Win32::System::RemoteDesktop::{
        WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, HWND_MESSAGE,
        PBT_APMPOWERSTATUSCHANGE, PBT_POWERSETTINGCHANGE, REGISTER_NOTIFICATION_FLAGS,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WNDCLASSEXW,
    };

    use super::{LPARAM, LRESULT, WPARAM};

    /// GUID_CONSOLE_DISPLAY_STATE: data is 0 = off, 1 = on, 2 = dimmed.
    const GUID_CONSOLE_DISPLAY_STATE: GUID =
        GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);
    /// GUID_ACDC_POWER_SOURCE: AC, DC (battery), or short-term UPS.
    const GUID_ACDC_POWER_SOURCE: GUID = GUID::from_u128(0x5d3e9a59_e9d5_4b00_a6bd_ff34ff516548);
    /// GUID_POWER_SAVING_STATUS: battery saver on/off.
    const GUID_POWER_SAVING_STATUS: GUID = GUID::from_u128(0xe00958c0_c213_4ace_ac77_fecced2eeea5);
    const CLASS_NAME: PCWSTR = w!("AuroraAnimationPower");

    thread_local! {
        static DISPLAY_OFF: Cell<bool> = const { Cell::new(false) };
        static CHANGED: Cell<bool> = const { Cell::new(false) };
    }

    /// Message-only window receiving display, power-source, battery-saver
    /// and lock/unlock notifications.
    pub struct PowerWatch {
        hwnd: HWND,
        registrations: Vec<HPOWERNOTIFY>,
        session: bool,
    }

    impl PowerWatch {
        pub fn register() -> Result<Self> {
            unsafe {
                let class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(power_wnd_proc),
                    lpszClassName: CLASS_NAME,
                    ..Default::default()
                };
                // A second registration fails harmlessly with "already exists".
                RegisterClassExW(&class);
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    CLASS_NAME,
                    PCWSTR::null(),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    None,
                    HINSTANCE::default(),
                    None,
                )
                .context("create power notification window")?;
                let mut watch = Self {
                    hwnd,
                    registrations: Vec::new(),
                    session: false,
                };
                for setting in [
                    GUID_CONSOLE_DISPLAY_STATE,
                    GUID_ACDC_POWER_SOURCE,
                    GUID_POWER_SAVING_STATUS,
                ] {
                    // DEVICE_NOTIFY_WINDOW_HANDLE; a missing setting only
                    // means slower reaction through the safety poll.
                    if let Ok(registration) = RegisterPowerSettingNotification(
                        HANDLE(hwnd.0),
                        &setting,
                        REGISTER_NOTIFICATION_FLAGS(0),
                    ) {
                        watch.registrations.push(registration);
                    }
                }
                watch.session =
                    WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_ok();
                Ok(watch)
            }
        }

        pub fn display_off(&self) -> bool {
            DISPLAY_OFF.with(Cell::get)
        }
    }

    /// True (once) if power, display, or session state changed since the
    /// last call.
    pub fn take_change() -> bool {
        CHANGED.with(|changed| changed.replace(false))
    }

    impl Drop for PowerWatch {
        fn drop(&mut self) {
            unsafe {
                for registration in self.registrations.drain(..) {
                    let _ = UnregisterPowerSettingNotification(registration);
                }
                if self.session {
                    let _ = WTSUnRegisterSessionNotification(self.hwnd);
                }
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    unsafe extern "system" fn power_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_POWERBROADCAST => {
                if wparam.0 == PBT_POWERSETTINGCHANGE as usize && lparam.0 != 0 {
                    let setting = &*(lparam.0 as *const POWERBROADCAST_SETTING);
                    if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE && setting.DataLength >= 1
                    {
                        DISPLAY_OFF.with(|off| off.set(setting.Data[0] == 0));
                    }
                    CHANGED.with(|changed| changed.set(true));
                } else if wparam.0 == PBT_APMPOWERSTATUSCHANGE as usize {
                    CHANGED.with(|changed| changed.set(true));
                }
                LRESULT(1)
            }
            WM_WTSSESSION_CHANGE => {
                CHANGED.with(|changed| changed.set(true));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_power_freeze_releases_frames_and_resume_redecodes() {
        let (results, commands) = mpsc::channel();
        let mut player = PlayerLoop::new(
            AnimatedConfig {
                enabled: true,
                ..AnimatedConfig::default()
            },
            commands,
            results,
            Arc::new(WakeEvent::new().unwrap()),
        );
        let target = DisplayTarget {
            monitor_id: "m".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            fit: WallpaperFit::Fill,
        };
        player.displays.insert(
            "m".into(),
            Display {
                target,
                path: PathBuf::from(r"C:\missing\loop.gif"),
                generation: 1,
                playing: Some(Playing {
                    animation: Animation {
                        width: 1,
                        height: 1,
                        frames: Vec::new(),
                        pixel_art: false,
                        background: [0, 0, 0, 255],
                    },
                    pixels_released: false,
                    window: None,
                    frame: 0,
                    next_due: Instant::now(),
                    frozen: true,
                    draw_failed: false,
                }),
                released: false,
                video: None,
                covered_since: None,
            },
        );
        player.next_generation = 1;
        let start = Instant::now();
        player.manage_long_freeze(true, start);
        assert!(
            player.displays["m"].playing.is_some(),
            "short freezes keep frames"
        );
        player.manage_long_freeze(true, start + RELEASE_FROZEN_AFTER);
        assert!(player.displays["m"].playing.is_none());
        assert!(player.displays["m"].released);
        assert!(player.any_active() && !player.any_playing());
        assert!(
            player.timeout(start).is_some(),
            "released displays keep policy checks"
        );

        player.manage_long_freeze(false, start + RELEASE_FROZEN_AFTER * 2);
        assert!(!player.displays["m"].released);
        assert_eq!(
            player.displays["m"].generation,
            1 + 1,
            "a new decode was started"
        );
    }

    #[test]
    fn device_reset_rebuilds_gpu_for_an_existing_gif_window() {
        use windows::core::{w, PCWSTR};
        use windows::Win32::Foundation::HINSTANCE;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
        };
        // The parent remains hidden: exercise actual window/device recovery
        // without attaching a test wallpaper to the user's desktop.
        struct HiddenParent(HWND);
        impl Drop for HiddenParent {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }
        let parent = HiddenParent(unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                8,
                8,
                HWND::default(),
                None,
                HINSTANCE::default(),
                None,
            )
            .unwrap()
        });
        let host = DesktopHost::Classic { workerw: parent.0 };
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        };
        let window = PlayerWindow::create(&host, bounds).unwrap();
        let (results, commands) = mpsc::channel();
        let mut player = PlayerLoop::new(
            AnimatedConfig {
                enabled: true,
                pause_on_battery: false,
                pause_when_covered: false,
                ..AnimatedConfig::default()
            },
            commands,
            results,
            Arc::new(WakeEvent::new().unwrap()),
        );
        player.host = Some(host);
        player.gpu = Some(Gpu::new().unwrap());
        player.displays.insert(
            "existing".into(),
            Display {
                target: DisplayTarget {
                    monitor_id: "existing".into(),
                    bounds,
                    fit: WallpaperFit::Fill,
                },
                path: PathBuf::from("fixture.gif"),
                generation: 1,
                video: None,
                released: false,
                covered_since: None,
                playing: Some(Playing {
                    animation: Animation {
                        width: 1,
                        height: 1,
                        pixel_art: false,
                        background: [0, 0, 0, 255],
                        frames: vec![frames::Frame {
                            bgra: vec![0, 0, 0, 255].into_boxed_slice(),
                            delay: Duration::from_millis(100),
                        }],
                    },
                    pixels_released: false,
                    window: Some(window),
                    frame: 0,
                    next_due: Instant::now(),
                    frozen: false,
                    draw_failed: false,
                }),
            },
        );
        player.reset_gpu();
        assert!(player.gpu.is_none());
        assert!(player.displays["existing"]
            .playing
            .as_ref()
            .unwrap()
            .window
            .is_some());
        player.apply_policy(Instant::now());
        assert!(
            player.gpu.is_some(),
            "an existing GIF window still needs its GPU rebuilt"
        );
        drop(player);
    }

    #[test]
    fn video_freeze_releases_and_recreates_stream_without_frame_cache() {
        let (results, commands) = mpsc::channel();
        let mut player = PlayerLoop::new(
            AnimatedConfig {
                enabled: true,
                ..AnimatedConfig::default()
            },
            commands,
            results,
            Arc::new(WakeEvent::new().unwrap()),
        );
        let target = DisplayTarget {
            monitor_id: "video".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1600,
            },
            fit: WallpaperFit::Fill,
        };
        player.handle(Command::Show(target, PathBuf::from("fixture.mp4")));
        assert!(player.displays["video"].video.is_some());
        assert!(player.displays["video"].playing.is_none());
        assert!(player.any_active());
        let start = Instant::now();
        player.manage_long_freeze(true, start);
        player.manage_long_freeze(true, start + RELEASE_FROZEN_AFTER);
        assert!(player.displays["video"].video.is_none());
        assert!(player.displays["video"].released);
        assert!(player.timeout(start).is_some());
        player.manage_long_freeze(false, start + RELEASE_FROZEN_AFTER * 2);
        assert!(player.displays["video"].video.is_some());
        assert!(!player.displays["video"].released);
        player.handle(Command::Hide("video".into()));
        assert!(!player.any_active());
    }

    #[test]
    fn covered_videos_release_after_a_delay_and_the_timer_restarts_when_visible() {
        let start = Instant::now();
        let mut since = None;
        assert!(!covered_long_enough(&mut since, true, start));
        assert!(!covered_long_enough(
            &mut since,
            true,
            start + RELEASE_COVERED_VIDEO_AFTER - Duration::from_secs(1)
        ));
        assert!(covered_long_enough(
            &mut since,
            true,
            start + RELEASE_COVERED_VIDEO_AFTER
        ));
        assert!(!covered_long_enough(
            &mut since,
            false,
            start + RELEASE_COVERED_VIDEO_AFTER * 2
        ));
        assert_eq!(since, None);
        assert!(
            !covered_long_enough(&mut since, true, start + RELEASE_COVERED_VIDEO_AFTER * 3),
            "a new cover starts a new timer"
        );
    }

    #[test]
    fn covered_release_boundary_and_cancellation_are_exact() {
        assert_eq!(RELEASE_COVERED_VIDEO_AFTER, Duration::from_secs(30));
        let start = Instant::now();
        let mut since = None;
        assert!(!covered_long_enough(&mut since, true, start));
        let almost = start + RELEASE_COVERED_VIDEO_AFTER - Duration::from_nanos(1);
        assert!(!covered_long_enough(&mut since, true, almost));
        // Uncover at the deadline must cancel, rather than release the player.
        let deadline = start + RELEASE_COVERED_VIDEO_AFTER;
        assert!(!covered_long_enough(&mut since, false, deadline));
        assert_eq!(since, None);
        assert!(!covered_long_enough(&mut since, true, deadline));
        assert!(!covered_long_enough(
            &mut since,
            true,
            deadline + Duration::from_secs(29)
        ));
        assert!(covered_long_enough(
            &mut since,
            true,
            deadline + Duration::from_secs(30)
        ));
    }

    #[test]
    fn repeated_cover_cycles_do_not_inherit_elapsed_time() {
        let start = Instant::now();
        let mut since = None;
        for cycle in 0..8 {
            let covered = start + Duration::from_secs(cycle * 100);
            assert!(!covered_long_enough(&mut since, true, covered));
            assert!(!covered_long_enough(
                &mut since,
                true,
                covered + Duration::from_secs(29)
            ));
            assert!(covered_long_enough(
                &mut since,
                true,
                covered + Duration::from_secs(30)
            ));
            assert!(covered_long_enough(
                &mut since,
                true,
                covered + Duration::from_secs(60)
            ));
            assert!(!covered_long_enough(
                &mut since,
                false,
                covered + Duration::from_secs(61)
            ));
            assert_eq!(since, None);
        }
    }

    #[test]
    fn disabled_config_creates_no_player() {
        assert!(AnimationPlayer::from_config(&AnimatedConfig::default()).is_none());
        let enabled = AnimatedConfig {
            enabled: true,
            ..AnimatedConfig::default()
        };
        let player = AnimationPlayer::from_config(&enabled).unwrap();
        assert!(player.thread.is_none(), "thread starts only on demand");
    }

    #[test]
    fn static_wallpapers_never_start_the_thread() {
        let mut player = AnimationPlayer::from_config(&AnimatedConfig {
            enabled: true,
            ..AnimatedConfig::default()
        })
        .unwrap();
        player.show(
            DisplayTarget {
                monitor_id: "m".into(),
                bounds: Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                fit: WallpaperFit::Fill,
            },
            PathBuf::from(r"C:\wallpapers\still.png"),
        );
        player.hide("m");
        player.hide_all();
        assert!(player.thread.is_none());
    }

    #[test]
    fn player_thread_reports_static_gif_and_shuts_down() {
        let dir = tempfile::tempdir().unwrap();
        let still = dir.path().join("still.gif");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]))
            .save(&still)
            .unwrap();
        let mut player = AnimationPlayer::from_config(&AnimatedConfig {
            enabled: true,
            ..AnimatedConfig::default()
        })
        .unwrap();
        player.show(
            DisplayTarget {
                monitor_id: "m".into(),
                bounds: Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                fit: WallpaperFit::Fill,
            },
            still,
        );
        assert!(player.thread.is_some());
        // Dropping joins the thread; this must not hang.
        drop(player);
    }
}

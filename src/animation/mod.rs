//! Opt-in animated wallpapers (GIF and animated WebP).
//!
//! The normal static commit through `IDesktopWallpaper` always happens first,
//! so Windows keeps showing the file's first frame whenever animation is off,
//! frozen, or unavailable. When `animated { enabled true }` is configured, a
//! single player thread draws the frames into a desktop-layer window per
//! display (see [`desktop`]).
//!
//! Cost model:
//! * disabled: no thread, no windows, no decoding;
//! * enabled with only static wallpapers: no thread;
//! * playing: one GDI blit per frame (at most `max-fps`), one policy check per
//!   second, and at most `max-memory-mb` of frames per display;
//! * frozen (fullscreen/maximized window, battery, battery saver, lock screen,
//!   display off): one policy check per second and no drawing.

pub mod desktop;
pub mod frames;
pub mod policy;
mod render;

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
use render::Gpu;

pub use frames::may_be_animated;

const POLICY_INTERVAL: Duration = Duration::from_secs(1);
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

    /// Play `path` on `target` if it is an animated GIF/WebP; otherwise make
    /// sure nothing covers that display's static wallpaper.
    pub fn show(&mut self, target: DisplayTarget, path: PathBuf) {
        if !may_be_animated(&path) {
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
    animation: Arc<Animation>,
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
    power: Option<power::DisplayPowerWatch>,
    /// Direct3D/Direct2D objects; present only while something animates.
    gpu: Option<Gpu>,
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
            power: None,
            gpu: None,
        }
    }

    fn run(mut self) {
        self.power = power::DisplayPowerWatch::register()
            .inspect_err(|error| debug!("display power notifications unavailable: {error:#}"))
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
            if self.any_playing() && now >= self.next_policy {
                self.apply_policy(now);
                self.next_policy = now + POLICY_INTERVAL;
            }
            self.advance_frames(Instant::now());
            let timeout = self.timeout(Instant::now());
            wait_and_pump(&self.wake, timeout);
        }
    }

    /// Returns false on shutdown.
    fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Show(target, path) => {
                self.next_generation += 1;
                let generation = self.next_generation;
                let budget = FrameBudget {
                    display_width: target.bounds.width,
                    display_height: target.bounds.height,
                    max_bytes: (self.config.max_memory_mb as usize) * 1024 * 1024,
                    max_frames: self.config.max_frames as usize,
                    min_delay: Duration::from_millis(1000 / u64::from(self.config.max_fps.max(1))),
                };
                let monitor_id = target.monitor_id.clone();
                // Replacing a display drops its old window right away; the
                // new file's first frame is already the static wallpaper.
                self.displays.insert(
                    monitor_id.clone(),
                    Display {
                        target,
                        path: path.clone(),
                        generation,
                        playing: None,
                    },
                );
                // Decode off this thread: it pumps messages for windows
                // parented to Explorer and must never stall.
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
                    animation: Arc::new(animation),
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
        let covered = if self.config.pause_when_covered && !frozen_everywhere {
            policy::covered_monitor()
        } else {
            None
        };

        // Explorer restarts destroy the shell windows and ours with them.
        if self.host.as_ref().is_some_and(|host| !host.is_alive()) {
            info!("desktop host changed; re-attaching animated wallpapers");
            self.host = None;
            for display in self.displays.values_mut() {
                if let Some(playing) = &mut display.playing {
                    playing.window = None;
                }
            }
        }

        for display in self.displays.values_mut() {
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
            let frozen = frozen_everywhere
                || covered.is_some_and(|rect| policy::same_rect(&rect, &display.target.bounds));
            if playing.frozen && !frozen {
                playing.next_due = now; // resume promptly, from the current frame
            }
            playing.frozen = frozen;
        }

        // Create windows lazily, and never while frozen, so a wallpaper that
        // starts under a fullscreen game costs nothing until it is visible.
        let needs_window = self.displays.values().any(|display| {
            display
                .playing
                .as_ref()
                .is_some_and(|playing| !playing.frozen && playing.window.is_none())
        });
        if needs_window && self.host.is_none() {
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
        let mut device_lost = false;
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
                        device_lost |= !draw(gpu, playing, display.target.fit);
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
        if device_lost {
            self.reset_gpu();
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
        let mut device_lost = false;
        for display in self.displays.values_mut() {
            let Some(playing) = &mut display.playing else {
                continue;
            };
            if playing.window.is_none() {
                continue;
            }
            if playing.frozen || now < playing.next_due {
                if repaint {
                    device_lost |= !draw(gpu, playing, display.target.fit);
                }
                continue;
            }
            let count = playing.animation.frames.len();
            playing.frame = (playing.frame + 1) % count;
            device_lost |= !draw(gpu, playing, display.target.fit);
            let delay = playing.animation.frames[playing.frame].delay;
            playing.next_due = if now.duration_since(playing.next_due) > MAX_FRAME_LAG {
                now + delay
            } else {
                playing.next_due + delay
            };
        }
        if device_lost {
            self.reset_gpu();
        }
    }

    /// How long the thread may sleep: forever when nothing plays.
    fn timeout(&self, now: Instant) -> Option<Duration> {
        let mut deadline: Option<Instant> = None;
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

/// Present the current frame. Returns false if the GPU device was lost.
fn draw(gpu: &Gpu, playing: &mut Playing, fit: WallpaperFit) -> bool {
    let Some(window) = &mut playing.window else {
        return true;
    };
    match window.show_frame(gpu, &playing.animation, playing.frame, fit) {
        Ok(()) => {
            playing.draw_failed = false;
            true
        }
        Err(error) => {
            if !playing.draw_failed {
                warn!("animated wallpaper frame failed; recreating GPU resources: {error:#}");
            }
            playing.draw_failed = true;
            false
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
    use std::cell::Cell;

    use anyhow::{Context, Result};
    use windows::core::{w, GUID, PCWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND};
    use windows::Win32::System::Power::{
        RegisterPowerSettingNotification, UnregisterPowerSettingNotification, HPOWERNOTIFY,
        POWERBROADCAST_SETTING,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, HWND_MESSAGE,
        PBT_POWERSETTINGCHANGE, REGISTER_NOTIFICATION_FLAGS, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_POWERBROADCAST, WNDCLASSEXW,
    };

    use super::{LPARAM, LRESULT, WPARAM};

    /// GUID_CONSOLE_DISPLAY_STATE: data is 0 = off, 1 = on, 2 = dimmed.
    const GUID_CONSOLE_DISPLAY_STATE: GUID =
        GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);
    const CLASS_NAME: PCWSTR = w!("AuroraAnimationPower");

    thread_local! {
        static DISPLAY_OFF: Cell<bool> = const { Cell::new(false) };
    }

    /// Message-only window receiving console display on/off notifications.
    pub struct DisplayPowerWatch {
        hwnd: HWND,
        registration: HPOWERNOTIFY,
    }

    impl DisplayPowerWatch {
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
                let registration = match RegisterPowerSettingNotification(
                    windows::Win32::Foundation::HANDLE(hwnd.0),
                    &GUID_CONSOLE_DISPLAY_STATE,
                    REGISTER_NOTIFICATION_FLAGS(0), // DEVICE_NOTIFY_WINDOW_HANDLE
                ) {
                    Ok(registration) => registration,
                    Err(error) => {
                        let _ = DestroyWindow(hwnd);
                        return Err(error).context("register display power notification");
                    }
                };
                Ok(Self { hwnd, registration })
            }
        }

        pub fn display_off(&self) -> bool {
            DISPLAY_OFF.with(Cell::get)
        }
    }

    impl Drop for DisplayPowerWatch {
        fn drop(&mut self) {
            unsafe {
                let _ = UnregisterPowerSettingNotification(self.registration);
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
        if msg == WM_POWERBROADCAST && wparam.0 == PBT_POWERSETTINGCHANGE as usize && lparam.0 != 0
        {
            let setting = &*(lparam.0 as *const POWERBROADCAST_SETTING);
            if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE && setting.DataLength >= 1 {
                DISPLAY_OFF.with(|off| off.set(setting.Data[0] == 0));
            }
            return LRESULT(1);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

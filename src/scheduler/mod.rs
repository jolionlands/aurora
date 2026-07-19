use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::config::types::ScheduleConfig;

pub const SWAP_QUEUE_CAPACITY: usize = 4;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SwapRequest {
    pub reason: SwapReason,
    /// If set, force this specific path (e.g. "next" command).
    pub specific: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapReason {
    Interval,
    AtTime,
    Manual,
    Previous,
    WorkspaceChange,
}

#[derive(Default)]
struct SchedulerProgressState {
    last_success: Option<Instant>,
    last_at_fired: Option<(u32, u32)>,
    pending_interval: bool,
    pending_at: Option<(u32, u32)>,
}

/// Completion state shared with the runtime. Queueing does not count as a
/// wallpaper change; only the runtime can record a successful apply.
#[derive(Clone, Default)]
pub struct SchedulerProgress(Arc<Mutex<SchedulerProgressState>>);

impl SchedulerProgress {
    pub fn complete(&self, reason: &SwapReason, succeeded: bool) {
        self.complete_at(reason, succeeded, Instant::now());
    }

    /// A policy pause intentionally consumes this scheduled opportunity rather
    /// than retrying every second as if the wallpaper apply had failed.
    pub fn defer(&self, reason: &SwapReason) {
        self.complete_at(reason, true, Instant::now());
    }

    fn complete_at(&self, reason: &SwapReason, succeeded: bool, now: Instant) {
        let mut state = self.0.lock();
        let at_slot = match reason {
            SwapReason::Interval => {
                state.pending_interval = false;
                None
            }
            SwapReason::AtTime => state.pending_at.take(),
            _ => None,
        };
        if succeeded {
            if !matches!(reason, SwapReason::Interval | SwapReason::AtTime) {
                state.pending_interval = false;
                if let Some(slot) = state.pending_at.take() {
                    state.last_at_fired = Some(slot);
                }
            }
            state.last_success = Some(now);
            if let Some(slot) = at_slot {
                state.last_at_fired = Some(slot);
            }
        }
    }

    fn begin_automatic(&self, reason: &SwapReason, at_slot: Option<(u32, u32)>) -> bool {
        let mut state = self.0.lock();
        match reason {
            SwapReason::Interval if !state.pending_interval => {
                state.pending_interval = true;
                true
            }
            SwapReason::AtTime
                if at_slot.is_some()
                    && state.pending_at.is_none()
                    && state.last_at_fired != at_slot =>
            {
                state.pending_at = at_slot;
                true
            }
            _ => false,
        }
    }

    fn cancel_automatic(&self, reason: &SwapReason) {
        let mut state = self.0.lock();
        match reason {
            SwapReason::Interval => state.pending_interval = false,
            SwapReason::AtTime => state.pending_at = None,
            _ => {}
        }
    }

    pub fn should_process(&self, reason: &SwapReason) -> bool {
        let state = self.0.lock();
        match reason {
            SwapReason::Interval => state.pending_interval,
            SwapReason::AtTime => state.pending_at.is_some(),
            _ => true,
        }
    }

    fn roll_minute(&self, current_hm: (u32, u32)) {
        let mut state = self.0.lock();
        if state.last_at_fired.is_some_and(|fired| fired != current_hm) {
            state.last_at_fired = None;
        }
    }

    fn interval_due(&self, interval: Duration, now: Instant) -> bool {
        let state = self.0.lock();
        !state.pending_interval
            && state
                .last_success
                .is_none_or(|last| now.duration_since(last) >= interval)
    }
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

pub struct Scheduler {
    config: ScheduleConfig,
    swap_tx: mpsc::Sender<SwapRequest>,
    progress: SchedulerProgress,
}

impl Scheduler {
    /// Construct scheduler + return the receiver end so callers can act on swaps.
    pub fn new(config: ScheduleConfig) -> (Self, mpsc::Receiver<SwapRequest>) {
        let (swap_tx, swap_rx) = mpsc::channel(SWAP_QUEUE_CAPACITY);
        let scheduler = Self {
            config,
            swap_tx,
            progress: SchedulerProgress::default(),
        };
        (scheduler, swap_rx)
    }

    pub fn sender(&self) -> mpsc::Sender<SwapRequest> {
        self.swap_tx.clone()
    }

    pub fn progress(&self) -> SchedulerProgress {
        self.progress.clone()
    }

    // -----------------------------------------------------------------------
    // Run loop
    // -----------------------------------------------------------------------

    /// Long-running async task.  Never returns unless cancelled.
    pub async fn run(&self) {
        let mut interval_ticker = tokio::time::interval(Duration::from_secs(1));
        let at_times = parse_at_times(&self.config.at_times);

        loop {
            interval_ticker.tick().await;

            // Fullscreen check
            if self.config.pause_when_fullscreen && is_fullscreen_active() {
                continue;
            }

            // Idle threshold means pause scheduling while the user is away.
            if self.config.pause_when_idle_secs > 0 {
                let idle_secs = get_idle_secs();
                if idle_secs >= self.config.pause_when_idle_secs as u64 {
                    // System is idle; skip scheduled swaps.
                    continue;
                }
            }

            // at_times check
            let current_hm = local_hour_minute();
            self.progress.roll_minute(current_hm);

            let should_at_fire = should_fire_at(&self.config.mode, &at_times, current_hm);

            if should_at_fire
                && self.try_enqueue_automatic(
                    SwapRequest {
                        reason: SwapReason::AtTime,
                        specific: None,
                    },
                    Some(current_hm),
                )
            {
                continue;
            }

            // Interval check
            if self.config.mode == "interval" {
                let interval = Duration::from_secs(self.config.interval_secs);
                if self.progress.interval_due(interval, Instant::now()) {
                    self.try_enqueue_automatic(
                        SwapRequest {
                            reason: SwapReason::Interval,
                            specific: None,
                        },
                        None,
                    );
                }
            }
        }
    }

    fn try_enqueue_automatic(&self, request: SwapRequest, at_slot: Option<(u32, u32)>) -> bool {
        if !self.progress.begin_automatic(&request.reason, at_slot) {
            return false;
        }
        match self.swap_tx.try_send(request) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(request)) => {
                self.progress.cancel_automatic(&request.reason);
                tracing::debug!(?request.reason, "swap queue full; coalescing automatic request");
                false
            }
            Err(mpsc::error::TrySendError::Closed(request)) => {
                self.progress.cancel_automatic(&request.reason);
                false
            }
        }
    }
}

pub(crate) fn checked_pause_deadline(duration: Option<Duration>) -> Option<Instant> {
    duration.and_then(|duration| Instant::now().checked_add(duration))
}

// ---------------------------------------------------------------------------
// at_times parsing
// ---------------------------------------------------------------------------

/// Parse "HH:MM" strings → (hour, minute) tuples.
pub fn parse_at_times(at_times: &[String]) -> Vec<(u32, u32)> {
    at_times.iter().filter_map(|s| parse_hhmm(s).ok()).collect()
}

fn should_fire_at(mode: &str, at_times: &[(u32, u32)], current_hm: (u32, u32)) -> bool {
    mode == "at" && at_times.contains(&current_hm)
}

pub fn parse_hhmm(s: &str) -> Result<(u32, u32)> {
    let (hour, minute) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid at_time format '{}': expected HH:MM", s))?;
    let h: u32 = hour
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid hour in '{}'", s))?;
    let m: u32 = minute
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid minute in '{}'", s))?;
    if h > 23 {
        bail!("hour out of range in '{}'", s);
    }
    if m > 59 {
        bail!("minute out of range in '{}'", s);
    }
    Ok((h, m))
}

// ---------------------------------------------------------------------------
// Windows platform helpers
// ---------------------------------------------------------------------------

fn local_hour_minute() -> (u32, u32) {
    let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (u32::from(now.wHour), u32::from(now.wMinute))
}

/// Returns true if the foreground window covers an entire monitor.
fn is_fullscreen_active() -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetShellWindow, GetWindowRect,
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        let is_shell_desktop = hwnd == GetShellWindow();

        let mut win_rect = RECT::default();
        if GetWindowRect(hwnd, &mut win_rect).is_err() {
            return false;
        }

        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return false;
        }

        let mr = mi.rcMonitor;
        should_pause_for_window(is_shell_desktop, &win_rect, &mr)
    }
}

fn should_pause_for_window(
    is_shell_desktop: bool,
    window: &windows::Win32::Foundation::RECT,
    monitor: &windows::Win32::Foundation::RECT,
) -> bool {
    !is_shell_desktop
        && window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom
}

/// Returns how many seconds the system has been idle (no keyboard/mouse input).
fn get_idle_secs() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !GetLastInputInfo(&mut lii).as_bool() {
            return 0;
        }
        idle_millis(GetTickCount(), lii.dwTime) as u64 / 1000
    }
}

fn idle_millis(now_ms: u32, last_input_ms: u32) -> u32 {
    now_ms.wrapping_sub(last_input_ms)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // at_times parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_at_time_parsing_valid() {
        assert_eq!(parse_hhmm("09:00").unwrap(), (9, 0));
        assert_eq!(parse_hhmm("23:59").unwrap(), (23, 59));
        assert_eq!(parse_hhmm("00:00").unwrap(), (0, 0));
        assert_eq!(parse_hhmm("12:30").unwrap(), (12, 30));
    }

    #[test]
    fn test_at_time_parsing_invalid() {
        assert!(parse_hhmm("bad").is_err());
        assert!(parse_hhmm("25:00").is_err());
        assert!(parse_hhmm("12:60").is_err());
        assert!(parse_hhmm("").is_err());
        assert!(parse_hhmm("1200").is_err());
    }

    #[test]
    fn at_times_only_fire_in_at_mode() {
        let times = [(9, 30)];
        assert!(!should_fire_at("interval", &times, (9, 30)));
        assert!(should_fire_at("at", &times, (9, 30)));
    }

    #[test]
    fn failed_scheduled_apply_is_immediately_retryable() {
        let progress = SchedulerProgress::default();
        assert!(progress.begin_automatic(&SwapReason::AtTime, Some((9, 30))));
        assert!(!progress.begin_automatic(&SwapReason::AtTime, Some((9, 30))));

        progress.complete(&SwapReason::AtTime, false);

        assert!(progress.begin_automatic(&SwapReason::AtTime, Some((9, 30))));
    }

    #[test]
    fn successful_at_time_is_suppressed_until_the_minute_changes() {
        let progress = SchedulerProgress::default();
        assert!(progress.begin_automatic(&SwapReason::AtTime, Some((9, 30))));
        progress.complete(&SwapReason::AtTime, true);
        assert!(!progress.begin_automatic(&SwapReason::AtTime, Some((9, 30))));

        progress.roll_minute((9, 31));

        assert!(progress.begin_automatic(&SwapReason::AtTime, Some((9, 30))));
    }

    #[test]
    fn only_successful_applies_reset_the_interval() {
        let progress = SchedulerProgress::default();
        let start = Instant::now();
        let interval = Duration::from_secs(60);
        assert!(progress.interval_due(interval, start));
        assert!(progress.begin_automatic(&SwapReason::Interval, None));
        progress.complete_at(&SwapReason::Interval, false, start);
        assert!(progress.interval_due(interval, start));

        assert!(progress.begin_automatic(&SwapReason::Interval, None));
        progress.complete_at(&SwapReason::Interval, true, start);
        assert!(!progress.interval_due(interval, start + Duration::from_secs(59)));
        assert!(progress.interval_due(interval, start + Duration::from_secs(60)));
    }

    #[test]
    fn successful_manual_change_postpones_interval_rotation() {
        let progress = SchedulerProgress::default();
        let start = Instant::now();
        assert!(progress.begin_automatic(&SwapReason::Interval, None));
        progress.complete_at(&SwapReason::Manual, true, start);

        assert!(!progress.interval_due(Duration::from_secs(60), start + Duration::from_secs(1)));
        assert!(!progress.should_process(&SwapReason::Interval));
    }

    #[test]
    fn successful_manual_change_consumes_queued_at_time_swap() {
        let progress = SchedulerProgress::default();
        assert!(progress.begin_automatic(&SwapReason::AtTime, Some((9, 30))));

        progress.complete(&SwapReason::Manual, true);

        assert!(!progress.should_process(&SwapReason::AtTime));
        assert!(!progress.begin_automatic(&SwapReason::AtTime, Some((9, 30))));
    }

    #[test]
    fn policy_pause_does_not_retry_an_automatic_request_every_tick() {
        let progress = SchedulerProgress::default();
        assert!(progress.begin_automatic(&SwapReason::Interval, None));
        progress.defer(&SwapReason::Interval);

        assert!(!progress.interval_due(Duration::from_secs(60), Instant::now()));
    }

    #[test]
    fn idle_time_handles_tick_count_rollover() {
        assert_eq!(idle_millis(500, u32::MAX - 499), 1_000);
    }

    #[test]
    fn local_time_is_valid() {
        let (hour, minute) = local_hour_minute();
        assert!(hour < 24);
        assert!(minute < 60);
    }

    #[test]
    fn fullscreen_pause_ignores_desktop_and_allows_invisible_borders() {
        use windows::Win32::Foundation::RECT;

        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let bordered_fullscreen = RECT {
            left: -8,
            top: -8,
            right: 1928,
            bottom: 1088,
        };
        assert!(should_pause_for_window(
            false,
            &bordered_fullscreen,
            &monitor
        ));
        assert!(!should_pause_for_window(
            true,
            &bordered_fullscreen,
            &monitor
        ));

        let inset_window = RECT {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1080,
        };
        assert!(!should_pause_for_window(false, &inset_window, &monitor));
    }

    #[test]
    fn full_queue_coalesces_automatic_requests() {
        let (scheduler, mut rx) = Scheduler::new(ScheduleConfig::default());
        assert!(scheduler.try_enqueue_automatic(
            SwapRequest {
                reason: SwapReason::Interval,
                specific: None,
            },
            None,
        ));
        assert!(!scheduler.try_enqueue_automatic(
            SwapRequest {
                reason: SwapReason::Interval,
                specific: None,
            },
            None,
        ));

        assert!(matches!(
            rx.try_recv().unwrap().reason,
            SwapReason::Interval
        ));
        assert!(rx.try_recv().is_err());
    }

    // -----------------------------------------------------------------------
    // Scheduler firing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_scheduler_interval_fires() {
        let config = ScheduleConfig {
            interval_secs: 1,
            mode: "interval".to_string(),
            pause_when_fullscreen: false,
            pause_when_idle_secs: 0,
            ..Default::default()
        };

        let (scheduler, mut rx) = Scheduler::new(config);

        // Run the scheduler in the background
        tokio::spawn(async move {
            scheduler.run().await;
        });

        // Wait up to 3 seconds for at least one swap request
        let result = tokio::time::timeout(Duration::from_millis(3000), rx.recv()).await;
        assert!(
            result.is_ok(),
            "should have received a swap request within 3s"
        );
        assert!(result.unwrap().is_some());
    }
}

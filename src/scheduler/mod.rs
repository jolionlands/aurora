use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use parking_lot::Mutex;
use tokio::sync::{mpsc, Notify};

use crate::config::types::ScheduleConfig;

pub const SWAP_QUEUE_CAPACITY: usize = 4;

/// First retry delay after a failed automatic swap; doubles per failure.
const FAILURE_BACKOFF_BASE: Duration = Duration::from_secs(30);
/// Longest retry delay after repeated failures.
const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);
/// While a due swap is held back by the fullscreen or idle policy, look again
/// this often.
const POLICY_RECHECK: Duration = Duration::from_secs(15);
/// Retry delay when a due swap could not be queued.
const QUEUE_FULL_RETRY: Duration = Duration::from_secs(1);
/// Upper bound on one sleep, so wall-clock changes (at-mode) and missed timer
/// wakeups after resume are noticed.
const MAX_SLEEP: Duration = Duration::from_secs(10 * 60);

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
    /// Consecutive failed automatic swaps; drives the retry backoff.
    failures: u32,
    /// No automatic swap before this instant (set after a failure).
    retry_at: Option<Instant>,
}

#[derive(Default)]
struct SchedulerProgressInner {
    state: Mutex<SchedulerProgressState>,
    /// Wakes the scheduler when a completion changes the next due time.
    changed: Notify,
}

/// Completion state shared with the runtime. Queueing does not count as a
/// wallpaper change; only the runtime can record a successful apply.
#[derive(Clone, Default)]
pub struct SchedulerProgress(Arc<SchedulerProgressInner>);

fn failure_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FAILURE_BACKOFF_BASE
        .saturating_mul(1 << doublings)
        .min(FAILURE_BACKOFF_MAX)
}

impl SchedulerProgress {
    /// Start interval cadence from daemon readiness when Windows already has
    /// a wallpaper, instead of replacing it immediately on every restart.
    pub fn seed_success(&self) {
        self.0
            .state
            .lock()
            .last_success
            .get_or_insert_with(Instant::now);
        self.0.changed.notify_one();
    }

    pub fn complete(&self, reason: &SwapReason, succeeded: bool) {
        self.complete_at(reason, succeeded, Instant::now());
    }

    /// A policy pause intentionally consumes this scheduled opportunity rather
    /// than retrying every second as if the wallpaper apply had failed.
    pub fn defer(&self, reason: &SwapReason) {
        self.complete_at(reason, true, Instant::now());
    }

    fn complete_at(&self, reason: &SwapReason, succeeded: bool, now: Instant) {
        let mut state = self.0.state.lock();
        let at_slot = match reason {
            SwapReason::Interval => {
                state.pending_interval = false;
                None
            }
            SwapReason::AtTime => state.pending_at.take(),
            _ => None,
        };
        let automatic = matches!(reason, SwapReason::Interval | SwapReason::AtTime);
        if !succeeded && automatic {
            // Back off instead of re-spawning helpers every tick while the
            // library, a playlist, or the shell keeps failing.
            state.failures = state.failures.saturating_add(1);
            state.retry_at = now.checked_add(failure_backoff(state.failures));
        }
        if succeeded {
            state.failures = 0;
            state.retry_at = None;
            if !automatic {
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
        drop(state);
        self.0.changed.notify_one();
    }

    fn begin_automatic(&self, reason: &SwapReason, at_slot: Option<(u32, u32)>) -> bool {
        let mut state = self.0.state.lock();
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
        let mut state = self.0.state.lock();
        match reason {
            SwapReason::Interval => state.pending_interval = false,
            SwapReason::AtTime => state.pending_at = None,
            _ => {}
        }
    }

    pub fn should_process(&self, reason: &SwapReason) -> bool {
        let state = self.0.state.lock();
        match reason {
            SwapReason::Interval => state.pending_interval,
            SwapReason::AtTime => state.pending_at.is_some(),
            _ => true,
        }
    }

    /// True if `slot` has neither fired successfully nor is queued.
    fn at_slot_open(&self, slot: (u32, u32)) -> bool {
        let state = self.0.state.lock();
        state.pending_at.is_none() && state.last_at_fired != Some(slot)
    }

    fn roll_minute(&self, current_hm: (u32, u32)) {
        let mut state = self.0.state.lock();
        if state.last_at_fired.is_some_and(|fired| fired != current_hm) {
            state.last_at_fired = None;
        }
    }

    fn interval_due(&self, interval: Duration, now: Instant) -> bool {
        self.next_interval_due(interval, now)
            .is_some_and(|due| due <= now)
    }

    /// When the next interval swap is due, or `None` while one is queued.
    fn next_interval_due(&self, interval: Duration, now: Instant) -> Option<Instant> {
        let state = self.0.state.lock();
        if state.pending_interval {
            return None;
        }
        let cadence = state
            .last_success
            .and_then(|last| last.checked_add(interval));
        match (cadence, state.retry_at) {
            (Some(cadence), Some(retry)) => Some(cadence.max(retry)),
            (Some(due), None) | (None, Some(due)) => Some(due),
            (None, None) => Some(now),
        }
    }

    /// Earliest instant an automatic at-time swap may be retried.
    fn retry_at(&self) -> Option<Instant> {
        self.0.state.lock().retry_at
    }

    async fn changed(&self) {
        self.0.changed.notified().await;
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
    ///
    /// Sleeps until the next swap is due instead of polling: an interval
    /// schedule wakes once per interval, an at-schedule once per configured
    /// time, and a completion from the runtime wakes it early to recompute.
    pub async fn run(&self) {
        let at_times = parse_at_times(&self.config.at_times);
        let interval = Duration::from_secs(self.config.interval_secs);

        loop {
            let now = Instant::now();
            let due = self.next_due(interval, &at_times, now);
            let sleep = due
                .map(|due| due.saturating_duration_since(now))
                .unwrap_or(MAX_SLEEP)
                .min(MAX_SLEEP);
            if !sleep.is_zero() {
                tokio::select! {
                    _ = tokio::time::sleep(sleep) => {}
                    _ = self.progress.changed() => {}
                }
                continue;
            }

            let retry = if self.policy_blocks() {
                POLICY_RECHECK
            } else if self.fire_due(interval, &at_times) {
                continue;
            } else {
                // Due but not queued (queue full of manual requests): wait
                // for the runtime instead of spinning.
                QUEUE_FULL_RETRY
            };
            tokio::select! {
                _ = tokio::time::sleep(retry) => {}
                _ = self.progress.changed() => {}
            }
        }
    }

    /// Fullscreen and idle policies hold back automatic swaps while active.
    fn policy_blocks(&self) -> bool {
        (self.config.pause_when_fullscreen && is_fullscreen_active())
            || (self.config.pause_when_idle_secs > 0
                && get_idle_secs() >= u64::from(self.config.pause_when_idle_secs))
    }

    /// The next instant something may be due, or `None` when only a runtime
    /// completion can make progress.
    fn next_due(
        &self,
        interval: Duration,
        at_times: &[(u32, u32)],
        now: Instant,
    ) -> Option<Instant> {
        match self.config.mode.as_str() {
            "interval" => self.progress.next_interval_due(interval, now),
            "at" => {
                let current = local_time();
                self.progress.roll_minute((current.0, current.1));
                if should_fire_at(&self.config.mode, at_times, (current.0, current.1))
                    && self.progress.at_slot_open((current.0, current.1))
                {
                    let retry = self.progress.retry_at().filter(|retry| *retry > now);
                    // A retry after the slot's minute has passed is moot.
                    let minute_left = Duration::from_secs(u64::from(60 - current.2.min(59)));
                    return match retry {
                        Some(retry) if retry.saturating_duration_since(now) < minute_left => {
                            Some(retry)
                        }
                        Some(_) => now.checked_add(minute_left),
                        None => Some(now),
                    };
                }
                now.checked_add(until_next_at_time(at_times, current))
            }
            _ => None,
        }
    }

    /// Queue whatever is due. Returns false if nothing could be queued.
    fn fire_due(&self, interval: Duration, at_times: &[(u32, u32)]) -> bool {
        let (hour, minute, _) = local_time();
        let current_hm = (hour, minute);
        self.progress.roll_minute(current_hm);
        if should_fire_at(&self.config.mode, at_times, current_hm)
            && self.try_enqueue_automatic(
                SwapRequest {
                    reason: SwapReason::AtTime,
                    specific: None,
                },
                Some(current_hm),
            )
        {
            return true;
        }
        self.config.mode == "interval"
            && self.progress.interval_due(interval, Instant::now())
            && self.try_enqueue_automatic(
                SwapRequest {
                    reason: SwapReason::Interval,
                    specific: None,
                },
                None,
            )
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

/// Local wall-clock `(hour, minute, second)`.
fn local_time() -> (u32, u32, u32) {
    let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (
        u32::from(now.wHour),
        u32::from(now.wMinute),
        u32::from(now.wSecond),
    )
}

/// Time from `current` until the start of the next configured `HH:MM`
/// minute (strictly later than the current minute).
fn until_next_at_time(
    at_times: &[(u32, u32)],
    (hour, minute, second): (u32, u32, u32),
) -> Duration {
    let now_minutes = hour * 60 + minute;
    let minutes = at_times
        .iter()
        .map(|(h, m)| {
            let slot = h * 60 + m;
            let ahead = (slot + 24 * 60 - now_minutes) % (24 * 60);
            if ahead == 0 {
                24 * 60
            } else {
                ahead
            }
        })
        .min()
        .unwrap_or(24 * 60);
    Duration::from_secs(u64::from(minutes) * 60)
        .saturating_sub(Duration::from_secs(u64::from(second.min(59))))
}

/// Returns true if the foreground window covers an entire monitor.
fn is_fullscreen_active() -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetShellWindow, GetWindowRect, IsZoomed,
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        let is_shell_desktop = hwnd == GetShellWindow();
        // A maximized window also covers the monitor when the taskbar is
        // auto-hidden; only borderless fullscreen (video, games) should pause.
        let is_maximized = IsZoomed(hwnd).as_bool();

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
        should_pause_for_window(is_shell_desktop, is_maximized, &win_rect, &mr)
    }
}

fn should_pause_for_window(
    is_shell_desktop: bool,
    is_maximized: bool,
    window: &windows::Win32::Foundation::RECT,
    monitor: &windows::Win32::Foundation::RECT,
) -> bool {
    !is_shell_desktop
        && !is_maximized
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
        // A failure retries after the backoff, not on the next tick.
        assert!(!progress.interval_due(interval, start));
        assert!(progress.interval_due(interval, start + FAILURE_BACKOFF_BASE));

        assert!(progress.begin_automatic(&SwapReason::Interval, None));
        progress.complete_at(&SwapReason::Interval, true, start);
        assert!(!progress.interval_due(interval, start + Duration::from_secs(59)));
        assert!(progress.interval_due(interval, start + Duration::from_secs(60)));
    }

    #[test]
    fn existing_wallpaper_starts_interval_cadence_without_an_immediate_swap() {
        let progress = SchedulerProgress::default();
        progress.seed_success();

        assert!(!progress.interval_due(Duration::from_secs(60), Instant::now()));
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
        let (hour, minute, second) = local_time();
        assert!(hour < 24);
        assert!(minute < 60);
        assert!(second < 61);
    }

    #[test]
    fn next_at_time_is_computed_not_polled() {
        let times = [(9, 30), (17, 0)];
        assert_eq!(
            until_next_at_time(&times, (9, 0, 0)),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            until_next_at_time(&times, (9, 29, 30)),
            Duration::from_secs(30)
        );
        // The current slot's minute never counts as "next".
        assert_eq!(
            until_next_at_time(&times, (9, 30, 0)),
            Duration::from_secs((17 * 60 - (9 * 60 + 30)) * 60)
        );
        // Wraps past midnight.
        assert_eq!(
            until_next_at_time(&times, (23, 0, 0)),
            Duration::from_secs((10 * 60 + 30) * 60)
        );
    }

    #[test]
    fn repeated_failures_back_off_exponentially_up_to_a_cap() {
        assert_eq!(failure_backoff(1), Duration::from_secs(30));
        assert_eq!(failure_backoff(2), Duration::from_secs(60));
        assert_eq!(failure_backoff(4), Duration::from_secs(240));
        assert_eq!(failure_backoff(50), FAILURE_BACKOFF_MAX);

        let progress = SchedulerProgress::default();
        let start = Instant::now();
        let interval = Duration::from_secs(3600);
        for _ in 0..3 {
            assert!(progress.begin_automatic(&SwapReason::Interval, None));
            progress.complete_at(&SwapReason::Interval, false, start);
        }
        assert!(!progress.interval_due(interval, start + Duration::from_secs(119)));
        assert!(progress.interval_due(interval, start + Duration::from_secs(120)));

        // A manual failure does not extend the automatic backoff, and any
        // success clears it.
        progress.complete_at(&SwapReason::Manual, false, start);
        assert!(progress.interval_due(interval, start + Duration::from_secs(120)));
        progress.complete_at(&SwapReason::Manual, true, start);
        assert!(!progress.interval_due(interval, start + Duration::from_secs(120)));
        assert!(progress.interval_due(interval, start + interval));
    }

    #[tokio::test]
    async fn completion_wakes_a_sleeping_scheduler() {
        let progress = SchedulerProgress::default();
        let waiter = {
            let progress = progress.clone();
            tokio::spawn(async move { progress.changed().await })
        };
        tokio::task::yield_now().await;
        progress.complete(&SwapReason::Manual, true);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("completion should wake the scheduler")
            .unwrap();
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
            false,
            &bordered_fullscreen,
            &monitor
        ));
        assert!(!should_pause_for_window(
            true,
            false,
            &bordered_fullscreen,
            &monitor
        ));
        // Maximized window with an auto-hidden taskbar covers the monitor too.
        assert!(!should_pause_for_window(
            false,
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
        assert!(!should_pause_for_window(
            false,
            false,
            &inset_window,
            &monitor
        ));
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

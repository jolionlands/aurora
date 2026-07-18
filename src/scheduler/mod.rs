use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
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

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

pub struct Scheduler {
    config: ScheduleConfig,
    swap_tx: mpsc::Sender<SwapRequest>,
}

impl Scheduler {
    /// Construct scheduler + return the receiver end so callers can act on swaps.
    pub fn new(config: ScheduleConfig) -> (Self, mpsc::Receiver<SwapRequest>) {
        let (swap_tx, swap_rx) = mpsc::channel(SWAP_QUEUE_CAPACITY);
        let scheduler = Self { config, swap_tx };
        (scheduler, swap_rx)
    }

    pub fn sender(&self) -> mpsc::Sender<SwapRequest> {
        self.swap_tx.clone()
    }

    // -----------------------------------------------------------------------
    // Run loop
    // -----------------------------------------------------------------------

    /// Long-running async task.  Never returns unless cancelled.
    pub async fn run(&self) {
        let mut interval_ticker = tokio::time::interval(Duration::from_secs(1));
        // Track which at_times we have already fired this minute to avoid double-fire.
        let mut last_at_fired: Option<(u32, u32)> = None;
        let mut last_swap = None;
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

            let should_at_fire =
                should_fire_at(&self.config.mode, &at_times, current_hm, last_at_fired);

            if should_at_fire {
                let enqueued = self.try_enqueue_automatic(SwapRequest {
                    reason: SwapReason::AtTime,
                    specific: None,
                });
                if record_at_enqueue(enqueued, current_hm, &mut last_at_fired, &mut last_swap) {
                    continue;
                }
            }

            // Reset last_at_fired when the minute has changed
            if let Some(fired) = last_at_fired {
                if fired != current_hm {
                    last_at_fired = None;
                }
            }

            // Interval check
            if self.config.mode == "interval" {
                let due = match last_swap {
                    None => true,
                    Some(last) => {
                        Instant::now().duration_since(last).as_secs() >= self.config.interval_secs
                    }
                };
                if due
                    && self.try_enqueue_automatic(SwapRequest {
                        reason: SwapReason::Interval,
                        specific: None,
                    })
                {
                    last_swap = Some(Instant::now());
                }
            }
        }
    }

    fn try_enqueue_automatic(&self, request: SwapRequest) -> bool {
        match self.swap_tx.try_send(request) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(request)) => {
                tracing::debug!(?request.reason, "swap queue full; coalescing automatic request");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

fn record_at_enqueue(
    enqueued: bool,
    current_hm: (u32, u32),
    last_at_fired: &mut Option<(u32, u32)>,
    last_swap: &mut Option<Instant>,
) -> bool {
    if enqueued {
        *last_at_fired = Some(current_hm);
        *last_swap = Some(Instant::now());
    }
    enqueued
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

fn should_fire_at(
    mode: &str,
    at_times: &[(u32, u32)],
    current_hm: (u32, u32),
    last_at_fired: Option<(u32, u32)>,
) -> bool {
    mode == "at" && at_times.contains(&current_hm) && last_at_fired != Some(current_hm)
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
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }

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
        win_rect.left == mr.left
            && win_rect.top == mr.top
            && win_rect.right == mr.right
            && win_rect.bottom == mr.bottom
    }
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
        assert!(!should_fire_at("interval", &times, (9, 30), None));
        assert!(should_fire_at("at", &times, (9, 30), None));
        assert!(!should_fire_at("at", &times, (9, 30), Some((9, 30))));
    }

    #[test]
    fn failed_at_enqueue_does_not_suppress_retry() {
        let mut last_at_fired = None;
        let mut last_swap = None;
        assert!(!record_at_enqueue(
            false,
            (9, 30),
            &mut last_at_fired,
            &mut last_swap
        ));
        assert_eq!(last_at_fired, None);
        assert_eq!(last_swap, None);

        assert!(record_at_enqueue(
            true,
            (9, 30),
            &mut last_at_fired,
            &mut last_swap
        ));
        assert_eq!(last_at_fired, Some((9, 30)));
        assert!(last_swap.is_some());
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
    fn full_queue_coalesces_automatic_requests() {
        let (scheduler, mut rx) = Scheduler::new(ScheduleConfig::default());
        for _ in 0..SWAP_QUEUE_CAPACITY {
            assert!(scheduler.try_enqueue_automatic(SwapRequest {
                reason: SwapReason::Interval,
                specific: None,
            }));
        }
        assert!(!scheduler.try_enqueue_automatic(SwapRequest {
            reason: SwapReason::WorkspaceChange,
            specific: None,
        }));

        for _ in 0..SWAP_QUEUE_CAPACITY {
            assert!(matches!(
                rx.try_recv().unwrap().reason,
                SwapReason::Interval
            ));
        }
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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use parking_lot::Mutex;
use tokio::sync::mpsc;

// TODO(audit): wire to crate::config::types::ScheduleConfig when foundation wires modules
use crate::config::types::ScheduleConfig;

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
    WorkspaceChange,
    OnIdle,
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub path: PathBuf,
    pub swapped_at: Instant,
    pub reason: SwapReason,
}

#[derive(Debug)]
pub struct SchedulerState {
    pub paused: bool,
    pub pause_until: Option<Instant>,
    pub last_swap: Option<Instant>,
    pub recent_paths: VecDeque<PathBuf>,
    pub history: VecDeque<HistoryEntry>,
}

impl SchedulerState {
    fn new() -> Self {
        Self {
            paused: false,
            pause_until: None,
            last_swap: None,
            recent_paths: VecDeque::new(),
            history: VecDeque::new(),
        }
    }

    fn is_effectively_paused(&mut self) -> bool {
        if let Some(until) = self.pause_until {
            if Instant::now() >= until {
                // timed pause expired — auto-resume
                self.paused = false;
                self.pause_until = None;
            }
        }
        self.paused
    }
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

pub struct Scheduler {
    config: ScheduleConfig,
    state: Arc<Mutex<SchedulerState>>,
    swap_tx: mpsc::UnboundedSender<SwapRequest>,
}

impl Scheduler {
    /// Construct scheduler + return the receiver end so callers can act on swaps.
    pub fn new(config: ScheduleConfig) -> (Self, mpsc::UnboundedReceiver<SwapRequest>) {
        let (swap_tx, swap_rx) = mpsc::unbounded_channel();
        let scheduler = Self {
            config,
            state: Arc::new(Mutex::new(SchedulerState::new())),
            swap_tx,
        };
        (scheduler, swap_rx)
    }

    // -----------------------------------------------------------------------
    // Control API
    // -----------------------------------------------------------------------

    /// Pause indefinitely, or for `duration` if provided.
    pub fn pause(&self, duration: Option<Duration>) {
        let mut st = self.state.lock();
        st.paused = true;
        st.pause_until = duration.map(|d| Instant::now() + d);
    }

    /// Resume from a pause immediately.
    pub fn resume(&self) {
        let mut st = self.state.lock();
        st.paused = false;
        st.pause_until = None;
    }

    /// Force an immediate swap (manual skip).
    pub fn skip(&self) {
        let _ = self.swap_tx.send(SwapRequest {
            reason: SwapReason::Manual,
            specific: None,
        });
    }

    /// Send a workspace-change swap.
    pub fn on_workspace_change(&self) {
        if self.config.on_workspace_change {
            let _ = self.swap_tx.send(SwapRequest {
                reason: SwapReason::WorkspaceChange,
                specific: None,
            });
        }
    }

    // -----------------------------------------------------------------------
    // Run loop
    // -----------------------------------------------------------------------

    /// Long-running async task.  Never returns unless cancelled.
    pub async fn run(&self) {
        let mut interval_ticker = tokio::time::interval(Duration::from_secs(1));
        // Track which at_times we have already fired this minute to avoid double-fire.
        let mut last_at_fired: Option<(u32, u32)> = None;

        loop {
            interval_ticker.tick().await;

            let mut st = self.state.lock();

            // Auto-resume timed pause
            if st.is_effectively_paused() {
                drop(st);
                continue;
            }

            // Fullscreen check
            if self.config.pause_when_fullscreen && is_fullscreen_active() {
                drop(st);
                continue;
            }

            // Idle check
            if self.config.pause_when_idle_secs > 0 {
                let idle_secs = get_idle_secs();
                if idle_secs >= self.config.pause_when_idle_secs as u64 {
                    // System is idle — fire if on_idle is configured, otherwise skip
                    let _ = self.swap_tx.send(SwapRequest {
                        reason: SwapReason::OnIdle,
                        specific: None,
                    });
                    st.last_swap = Some(Instant::now());
                    drop(st);
                    continue;
                }
            }

            // at_times check
            let now_local = chrono::Local::now();
            let current_hm = (now_local.format("%H").to_string().parse::<u32>().unwrap_or(0),
                              now_local.format("%M").to_string().parse::<u32>().unwrap_or(0));

            let at_times = parse_at_times(&self.config.at_times);
            let should_at_fire = at_times.iter().any(|&hm| hm == current_hm)
                && last_at_fired != Some(current_hm);

            if should_at_fire {
                last_at_fired = Some(current_hm);
                let _ = self.swap_tx.send(SwapRequest {
                    reason: SwapReason::AtTime,
                    specific: None,
                });
                st.last_swap = Some(Instant::now());
                drop(st);
                continue;
            }

            // Reset last_at_fired when the minute has changed
            if let Some(fired) = last_at_fired {
                if fired != current_hm {
                    last_at_fired = None;
                }
            }

            // Interval check
            if self.config.mode == "interval" || self.config.mode.is_empty() {
                let due = match st.last_swap {
                    None => true,
                    Some(last) => Instant::now().duration_since(last).as_secs()
                        >= self.config.interval_secs,
                };
                if due {
                    let _ = self.swap_tx.send(SwapRequest {
                        reason: SwapReason::Interval,
                        specific: None,
                    });
                    st.last_swap = Some(Instant::now());
                }
            }

            drop(st);
        }
    }

    pub fn state(&self) -> Arc<Mutex<SchedulerState>> {
        Arc::clone(&self.state)
    }
}

// ---------------------------------------------------------------------------
// at_times parsing
// ---------------------------------------------------------------------------

/// Parse "HH:MM" strings → (hour, minute) tuples.
pub fn parse_at_times(at_times: &[String]) -> Vec<(u32, u32)> {
    at_times
        .iter()
        .filter_map(|s| parse_hhmm(s).ok())
        .collect()
}

pub fn parse_hhmm(s: &str) -> Result<(u32, u32)> {
    let parts: Vec<&str> = s.splitn(2, ':').collect();
    if parts.len() != 2 {
        bail!("invalid at_time format '{}': expected HH:MM", s);
    }
    let h: u32 = parts[0].parse().map_err(|_| anyhow::anyhow!("invalid hour in '{}'", s))?;
    let m: u32 = parts[1].parse().map_err(|_| anyhow::anyhow!("invalid minute in '{}'", s))?;
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

/// Returns true if the foreground window covers an entire monitor.
fn is_fullscreen_active() -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowRect,
        };
        use windows::Win32::Graphics::Gdi::{
            MonitorFromWindow, GetMonitorInfoW, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };

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
            if GetMonitorInfoW(hmon, &mut mi).is_err() {
                return false;
            }

            let mr = mi.rcMonitor;
            win_rect.left == mr.left
                && win_rect.top == mr.top
                && win_rect.right == mr.right
                && win_rect.bottom == mr.bottom
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Returns how many seconds the system has been idle (no keyboard/mouse input).
fn get_idle_secs() -> u64 {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        use windows::Win32::System::SystemInformation::GetTickCount;

        unsafe {
            let mut lii = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };
            if GetLastInputInfo(&mut lii).is_err() {
                return 0;
            }
            let now_ms = GetTickCount() as u64;
            let last_ms = lii.dwTime as u64;
            now_ms.saturating_sub(last_ms) / 1000
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        0
    }
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

    // -----------------------------------------------------------------------
    // Scheduler firing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_scheduler_interval_fires() {
        let mut config = ScheduleConfig::default();
        config.interval_secs = 1; // 1-second interval
        config.mode = "interval".to_string();
        config.pause_when_fullscreen = false;
        config.pause_when_idle_secs = 0;

        let (scheduler, mut rx) = Scheduler::new(config);

        // Run the scheduler in the background
        tokio::spawn(async move {
            scheduler.run().await;
        });

        // Wait up to 3 seconds for at least one swap request
        let result = tokio::time::timeout(Duration::from_millis(3000), rx.recv()).await;
        assert!(result.is_ok(), "should have received a swap request within 3s");
        assert!(result.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_scheduler_pause_blocks_fire() {
        let mut config = ScheduleConfig::default();
        config.interval_secs = 1;
        config.mode = "interval".to_string();
        config.pause_when_fullscreen = false;
        config.pause_when_idle_secs = 0;

        let (scheduler, mut rx) = Scheduler::new(config);
        // Pause before starting
        scheduler.pause(None);

        tokio::spawn(async move {
            scheduler.run().await;
        });

        // After 2s, should have received nothing
        let result =
            tokio::time::timeout(Duration::from_millis(2000), rx.recv()).await;
        assert!(result.is_err(), "paused scheduler should not fire");
    }
}

//! When animated wallpapers should freeze to save power.

use windows::Win32::Foundation::RECT;

use crate::config::types::AnimatedConfig;
use crate::transition::Rect;

/// Machine-wide conditions sampled once per policy check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemState {
    pub on_battery: bool,
    pub battery_saver: bool,
    /// The secure desktop (lock screen, UAC) owns input.
    pub locked: bool,
    /// The console display is off (reported by a power notification).
    pub display_off: bool,
}

/// Global freeze decision: true when no display should animate.
pub fn pause_everywhere(config: &AnimatedConfig, state: SystemState) -> bool {
    state.battery_saver
        || state.locked
        || state.display_off
        || (config.pause_on_battery && state.on_battery)
}

pub fn sample_system_state(display_off: bool) -> SystemState {
    let (on_battery, battery_saver) = power_status();
    SystemState {
        on_battery,
        battery_saver,
        locked: input_desktop_locked(),
        display_off,
    }
}

fn power_status() -> (bool, bool) {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut status = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return (false, false);
    }
    // ACLineStatus: 0 offline, 1 online, 255 unknown (desktops report 1).
    (status.ACLineStatus == 0, status.SystemStatusFlag == 1)
}

fn input_desktop_locked() -> bool {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
    };

    // A normal user process cannot open Winlogon's secure desktop.
    match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) } {
        Ok(desktop) => {
            let _ = unsafe { CloseDesktop(desktop) };
            false
        }
        Err(_) => true,
    }
}

/// The foreground window's monitor rectangle, when that window is maximized
/// or fullscreen and therefore hides the desktop on that monitor.
pub fn covered_monitor() -> Option<Rect> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed,
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        let mut class = [0u16; 32];
        let len = GetClassNameW(hwnd, &mut class) as usize;
        if is_desktop_class(&String::from_utf16_lossy(&class[..len.min(class.len())])) {
            return None;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let mut window = RECT::default();
        if GetWindowRect(hwnd, &mut window).is_err() {
            return None;
        }
        covers(
            IsZoomed(hwnd).as_bool(),
            &window,
            &info.rcMonitor,
            &info.rcWork,
        )
        .then(|| rect_from(&info.rcMonitor))
    }
}

fn is_desktop_class(class: &str) -> bool {
    matches!(
        class,
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

/// Maximized windows hide the work area; borderless fullscreen windows cover
/// the whole monitor.
fn covers(maximized: bool, window: &RECT, monitor: &RECT, work: &RECT) -> bool {
    let contains = |outer: &RECT, inner: &RECT| {
        outer.left <= inner.left
            && outer.top <= inner.top
            && outer.right >= inner.right
            && outer.bottom >= inner.bottom
    };
    contains(window, monitor) || (maximized && contains(window, work))
}

fn rect_from(rect: &RECT) -> Rect {
    Rect {
        x: rect.left,
        y: rect.top,
        width: (rect.right - rect.left).max(0) as u32,
        height: (rect.bottom - rect.top).max(0) as u32,
    }
}

pub fn same_rect(a: &Rect, b: &Rect) -> bool {
    (a.x, a.y, a.width, a.height) == (b.x, b.y, b.width, b.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_saver_lock_and_display_off_always_pause() {
        let config = AnimatedConfig {
            pause_on_battery: false,
            ..AnimatedConfig::default()
        };
        assert!(!pause_everywhere(&config, SystemState::default()));
        for state in [
            SystemState {
                battery_saver: true,
                ..Default::default()
            },
            SystemState {
                locked: true,
                ..Default::default()
            },
            SystemState {
                display_off: true,
                ..Default::default()
            },
        ] {
            assert!(pause_everywhere(&config, state), "{state:?}");
        }
    }

    #[test]
    fn battery_pause_is_configurable() {
        let on_battery = SystemState {
            on_battery: true,
            ..Default::default()
        };
        assert!(pause_everywhere(&AnimatedConfig::default(), on_battery));
        let config = AnimatedConfig {
            pause_on_battery: false,
            ..AnimatedConfig::default()
        };
        assert!(!pause_everywhere(&config, on_battery));
    }

    #[test]
    fn maximized_or_fullscreen_windows_cover_the_desktop() {
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 800,
        };
        let work = RECT {
            bottom: 752,
            ..monitor
        };
        let maximized = RECT {
            left: -8,
            top: -8,
            right: 1288,
            bottom: 760,
        };
        assert!(covers(true, &maximized, &monitor, &work));
        assert!(!covers(false, &maximized, &monitor, &work));
        assert!(covers(false, &monitor, &monitor, &work));
        let windowed = RECT {
            left: 100,
            top: 100,
            right: 900,
            bottom: 700,
        };
        assert!(!covers(true, &windowed, &monitor, &work));
        assert!(is_desktop_class("Progman"));
        assert!(!is_desktop_class("Chrome_WidgetWin_1"));
    }
}

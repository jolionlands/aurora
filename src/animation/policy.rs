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

/// A display counts as covered when windows leave less than this share of its
/// work area visible, as snapped or tiled layouts do.
const MIN_VISIBLE_DESKTOP: f64 = 0.02;

/// Monitors hidden behind a visible maximized or fullscreen window, or behind
/// windows that together leave almost none of the desktop visible.
///
/// Every top-level window is considered, not just the foreground one: a
/// maximized editor stays in front of the desktop while a small dialog has
/// focus. Cloaked windows (other virtual desktops, suspended UWP) and
/// click-through overlays do not count.
pub fn covered_monitors() -> Vec<Rect> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;

    #[derive(Default)]
    struct Scan {
        covered: Vec<Rect>,
        frames: Vec<RECT>,
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let scan = &mut *(lparam.0 as *mut Scan);
        if let Some((frame, alone)) = opaque_window(hwnd) {
            scan.frames.push(frame);
            if let Some(rect) = alone {
                if !scan.covered.iter().any(|known| same_rect(known, &rect)) {
                    scan.covered.push(rect);
                }
            }
        }
        BOOL(1)
    }

    let mut scan = Scan::default();
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut scan as *mut Scan as isize));
    }
    for (monitor, work) in monitors() {
        let rect = rect_from(&monitor);
        if !scan.covered.iter().any(|known| same_rect(known, &rect))
            && hidden_by_windows(&work, &scan.frames)
        {
            scan.covered.push(rect);
        }
    }
    scan.covered
}

/// Every display's full and work-area rectangles.
fn monitors() -> Vec<(RECT, RECT)> {
    use windows::Win32::Foundation::{BOOL, LPARAM};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };

    unsafe extern "system" fn visit(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let found = &mut *(lparam.0 as *mut Vec<(RECT, RECT)>);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            found.push((info.rcMonitor, info.rcWork));
        }
        BOOL(1)
    }

    let mut found = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(visit),
            LPARAM(&mut found as *mut Vec<(RECT, RECT)> as isize),
        );
    }
    found
}

/// Windows leave less than [`MIN_VISIBLE_DESKTOP`] of `work` visible.
fn hidden_by_windows(work: &RECT, windows: &[RECT]) -> bool {
    visible_fraction(work, windows) < MIN_VISIBLE_DESKTOP
}

/// Share of `work` that no rectangle in `windows` overlaps.
fn visible_fraction(work: &RECT, windows: &[RECT]) -> f64 {
    let area =
        |r: &RECT| f64::from((r.right - r.left).max(0)) * f64::from((r.bottom - r.top).max(0));
    let total = area(work);
    if total == 0.0 {
        return 0.0;
    }
    let clipped: Vec<RECT> = windows
        .iter()
        .map(|w| RECT {
            left: w.left.max(work.left),
            top: w.top.max(work.top),
            right: w.right.min(work.right),
            bottom: w.bottom.min(work.bottom),
        })
        .filter(|r| r.left < r.right && r.top < r.bottom)
        .collect();
    // Split the work area at every window edge: each cell is then wholly
    // inside or outside every window, so overlaps count once.
    let edges = |a: i32, b: i32, pick: fn(&RECT) -> [i32; 2]| {
        let mut edges: Vec<i32> = clipped.iter().flat_map(pick).chain([a, b]).collect();
        edges.sort_unstable();
        edges.dedup();
        edges
    };
    let xs = edges(work.left, work.right, |r| [r.left, r.right]);
    let ys = edges(work.top, work.bottom, |r| [r.top, r.bottom]);
    let mut hidden = 0.0;
    for x in xs.windows(2) {
        for y in ys.windows(2) {
            let inside =
                |r: &RECT| r.left <= x[0] && x[0] < r.right && r.top <= y[0] && y[0] < r.bottom;
            if clipped.iter().any(inside) {
                hidden += f64::from(x[1] - x[0]) * f64::from(y[1] - y[0]);
            }
        }
    }
    1.0 - hidden / total
}

/// A visible, opaque, uncloaked application window: its visible frame, and
/// the monitor it hides on its own if it is maximized or fullscreen.
unsafe fn opaque_window(hwnd: windows::Win32::Foundation::HWND) -> Option<(RECT, Option<Rect>)> {
    use windows::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    };
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowLongW, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed,
        GWL_EXSTYLE, WS_EX_TRANSPARENT,
    };

    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return None;
    }
    if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT.0 != 0 {
        return None; // click-through overlay
    }
    let mut cloaked = 0u32;
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        (&mut cloaked as *mut u32).cast(),
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && cloaked != 0
    {
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
    GetWindowRect(hwnd, &mut window).ok()?;
    // The visible frame, without the invisible resize borders.
    let mut frame = window;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        (&mut frame as *mut RECT).cast(),
        std::mem::size_of::<RECT>() as u32,
    );
    let alone = covers(
        IsZoomed(hwnd).as_bool(),
        &window,
        &info.rcMonitor,
        &info.rcWork,
    )
    .then(|| rect_from(&info.rcMonitor));
    Some((frame, alone))
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
    fn covered_monitor_scan_is_fast_and_sane() {
        let started = std::time::Instant::now();
        let covered = covered_monitors();
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        assert!(covered.iter().all(|rect| rect.width > 0 && rect.height > 0));
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

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn snapped_and_tiled_windows_hide_the_desktop_together() {
        let work = rect(0, 0, 1280, 752);
        assert_eq!(visible_fraction(&work, &[]), 1.0);
        let halves = [rect(0, 0, 640, 752), rect(640, 0, 1280, 752)];
        assert_eq!(visible_fraction(&work, &halves), 0.0);
        assert!(hidden_by_windows(&work, &halves));

        // Three tiles with a thin gutter still count as covered.
        let tiles = [
            rect(0, 0, 638, 752),
            rect(642, 0, 1280, 374),
            rect(642, 378, 1280, 752),
        ];
        assert!(hidden_by_windows(&work, &tiles));
    }

    #[test]
    fn partly_visible_desktops_keep_playing() {
        let work = rect(0, 0, 1000, 1000);
        let left = rect(0, 0, 600, 1000);
        assert!((visible_fraction(&work, &[left]) - 0.4).abs() < 1e-9);
        // Overlaps count once; windows on other monitors do not count.
        let overlapping = [left, rect(300, 0, 700, 1000), rect(1000, 0, 2000, 1000)];
        assert!((visible_fraction(&work, &overlapping) - 0.3).abs() < 1e-9);
        assert!(!hidden_by_windows(&work, &overlapping));
        // A small visible corner is enough to keep animating.
        let corner_open = [rect(0, 0, 1000, 800), rect(0, 800, 750, 1000)];
        assert!((visible_fraction(&work, &corner_open) - 0.05).abs() < 1e-9);
        assert!(!hidden_by_windows(&work, &corner_open));
    }
}

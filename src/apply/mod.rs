use anyhow::{Context, Result};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_LOCAL_SERVER};
use windows::Win32::UI::Shell::{
    DesktopWallpaper, IDesktopWallpaper, DESKTOP_WALLPAPER_POSITION, DWPOS_CENTER, DWPOS_FILL,
    DWPOS_FIT, DWPOS_SPAN, DWPOS_STRETCH, DWPOS_TILE,
};

use crate::config::types::Config;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Device path string, e.g. `\\.\DISPLAY1\Monitor0`.
    pub id: String,
    /// Physical monitor origin in virtual-screen coordinates.
    pub x: i32,
    /// Physical monitor origin in virtual-screen coordinates.
    pub y: i32,
    /// Physical monitor width in pixels.
    pub width: u32,
    /// Physical monitor height in pixels.
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallpaperFit {
    Fill,
    Contain,
    Tile,
    Center,
    Stretch,
    Span,
}

impl WallpaperFit {
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "fill" => WallpaperFit::Fill,
            "contain" | "fit" => WallpaperFit::Contain,
            "tile" => WallpaperFit::Tile,
            "center" | "centre" => WallpaperFit::Center,
            "stretch" => WallpaperFit::Stretch,
            "span" => WallpaperFit::Span,
            _ => WallpaperFit::Fill,
        }
    }

    fn to_dwpos(self) -> DESKTOP_WALLPAPER_POSITION {
        match self {
            WallpaperFit::Fill => DWPOS_FILL,
            WallpaperFit::Contain => DWPOS_FIT,
            WallpaperFit::Tile => DWPOS_TILE,
            WallpaperFit::Center => DWPOS_CENTER,
            WallpaperFit::Stretch => DWPOS_STRETCH,
            WallpaperFit::Span => DWPOS_SPAN,
        }
    }
}

// ---------------------------------------------------------------------------
// WallpaperApplier
// ---------------------------------------------------------------------------

pub struct WallpaperApplier {
    desktop: IDesktopWallpaper,
}

impl WallpaperApplier {
    pub fn new() -> Result<Self> {
        let desktop: IDesktopWallpaper = unsafe {
            CoCreateInstance(&DesktopWallpaper, None, CLSCTX_LOCAL_SERVER)
                .context("CoCreateInstance(DesktopWallpaper)")?
        };
        Ok(Self { desktop })
    }

    /// Enumerate all monitors known to the shell wallpaper API.
    pub fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        use windows::Win32::System::Com::CoTaskMemFree;

        let count = unsafe {
            self.desktop
                .GetMonitorDevicePathCount()
                .context("IDesktopWallpaper::GetMonitorDevicePathCount")?
        };

        let mut monitors = Vec::with_capacity(count as usize);
        for i in 0..count {
            let pwstr = unsafe {
                self.desktop
                    .GetMonitorDevicePathAt(i)
                    .with_context(|| format!("GetMonitorDevicePathAt({})", i))?
            };
            // PWSTR::to_string() returns Result<String, FromUtf16Error>
            let id = unsafe {
                let id = pwstr
                    .to_string()
                    .unwrap_or_else(|_| format!("monitor-{}", i));
                CoTaskMemFree(Some(pwstr.0.cast()));
                id
            };
            let monitor_wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0u16)).collect();
            let rect = unsafe {
                self.desktop
                    .GetMonitorRECT(PCWSTR::from_raw(monitor_wide.as_ptr()))
                    .with_context(|| format!("GetMonitorRECT({})", id))?
            };
            let width = rect.right.saturating_sub(rect.left);
            let height = rect.bottom.saturating_sub(rect.top);
            if width <= 0 || height <= 0 {
                anyhow::bail!("GetMonitorRECT({}) returned an empty rectangle", id);
            }
            monitors.push(MonitorInfo {
                id,
                x: rect.left,
                y: rect.top,
                width: width as u32,
                height: height as u32,
            });
        }
        Ok(monitors)
    }

    /// Set the wallpaper on a specific monitor.
    pub fn set_for_monitor(&self, monitor_id: &str, path: &Path) -> Result<()> {
        let monitor_wide: Vec<u16> = monitor_id
            .encode_utf16()
            .chain(std::iter::once(0u16))
            .collect();
        let path_wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0u16))
            .collect();

        unsafe {
            self.desktop
                .SetWallpaper(
                    PCWSTR::from_raw(monitor_wide.as_ptr()),
                    PCWSTR::from_raw(path_wide.as_ptr()),
                )
                .with_context(|| {
                    format!(
                        "IDesktopWallpaper::SetWallpaper(monitor={}, path={})",
                        monitor_id,
                        path.display()
                    )
                })
        }
    }

    pub fn set_for_all_monitors(&self, path: &Path) -> Result<()> {
        let path_wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0u16))
            .collect();
        unsafe {
            self.desktop
                .SetWallpaper(PCWSTR::null(), PCWSTR::from_raw(path_wide.as_ptr()))
                .with_context(|| {
                    format!(
                        "IDesktopWallpaper::SetWallpaper(all monitors, path={})",
                        path.display()
                    )
                })
        }
    }

    /// Set the fit/position mode for all monitors.
    pub fn set_fit(&self, fit: WallpaperFit) -> Result<()> {
        let position = fit.to_dwpos();
        unsafe {
            if self.desktop.GetPosition().ok() == Some(position) {
                return Ok(());
            }
            self.desktop
                .SetPosition(position)
                .context("IDesktopWallpaper::SetPosition")
        }
    }

    pub fn apply_all(&self, config: &Config, path: &Path) -> Result<()> {
        if let Some(first) = config.monitors.first() {
            let fit = WallpaperFit::parse(&first.fit);
            if config
                .monitors
                .iter()
                .skip(1)
                .any(|monitor| WallpaperFit::parse(&monitor.fit) != fit)
            {
                tracing::warn!(
                    "per-monitor fit overrides differ, but Windows applies one global wallpaper position"
                );
            }
            self.set_fit(fit)?;
        }
        self.set_for_all_monitors(path)
    }
}

pub fn configured_global_fit(config: &Config, monitors: &[MonitorInfo]) -> WallpaperFit {
    let fit_for = |monitor_id: &str| {
        config
            .monitors
            .iter()
            .find(|monitor| monitor.name == monitor_id)
            .map(|monitor| WallpaperFit::parse(&monitor.fit))
            .unwrap_or(WallpaperFit::Fill)
    };
    let fit = fit_for(&monitors[0].id);
    if monitors
        .iter()
        .skip(1)
        .any(|monitor| fit_for(&monitor.id) != fit)
    {
        tracing::warn!(
            "per-monitor fit overrides differ, but Windows applies one global wallpaper position"
        );
    }
    fit
}

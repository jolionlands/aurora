use anyhow::{Context, Result};
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_LOCAL_SERVER};
use windows::Win32::UI::Shell::{
    DesktopWallpaper, IDesktopWallpaper, DESKTOP_WALLPAPER_POSITION, DWPOS_CENTER, DWPOS_FILL,
    DWPOS_FIT, DWPOS_SPAN, DWPOS_STRETCH, DWPOS_TILE,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Device path string, e.g. `\\.\DISPLAY1\Monitor0`.
    pub id: String,
    /// 0-based display index as returned by the shell API.
    pub index: u32,
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

// SAFETY: see comment in decode/mod.rs — COM MTA proxy makes this safe for
// our single-owner pattern.
unsafe impl Send for WallpaperApplier {}

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
                index: i,
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
        let path_str = path.to_string_lossy();
        let monitor_wide: Vec<u16> = monitor_id
            .encode_utf16()
            .chain(std::iter::once(0u16))
            .collect();
        let path_wide: Vec<u16> = path_str
            .encode_utf16()
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
                        monitor_id, path_str
                    )
                })
        }
    }

    /// Set the same wallpaper on all monitors.
    pub fn set_for_all(&self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy();
        let path_wide: Vec<u16> = path_str
            .encode_utf16()
            .chain(std::iter::once(0u16))
            .collect();

        unsafe {
            // PCWSTR::null() for the monitor ID applies the wallpaper to all monitors.
            self.desktop
                .SetWallpaper(PCWSTR::null(), PCWSTR::from_raw(path_wide.as_ptr()))
                .with_context(|| format!("IDesktopWallpaper::SetWallpaper(all, path={})", path_str))
        }
    }

    /// Get the current wallpaper path for a specific monitor.
    pub fn get_current(&self, monitor_id: &str) -> Result<Option<std::path::PathBuf>> {
        use windows::Win32::System::Com::CoTaskMemFree;

        let monitor_wide: Vec<u16> = monitor_id
            .encode_utf16()
            .chain(std::iter::once(0u16))
            .collect();

        let pwstr = unsafe {
            match self
                .desktop
                .GetWallpaper(PCWSTR::from_raw(monitor_wide.as_ptr()))
            {
                Ok(p) => p,
                Err(e) => {
                    // E_INVALIDARG (0x80070057) when the monitor has no wallpaper set
                    if e.code().0 == 0x80070057u32 as i32 {
                        return Ok(None);
                    }
                    return Err(e).context("IDesktopWallpaper::GetWallpaper");
                }
            }
        };

        let s = unsafe {
            let s = pwstr.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pwstr.0.cast()));
            s
        };
        if s.is_empty() {
            Ok(None)
        } else {
            Ok(Some(std::path::PathBuf::from(s)))
        }
    }

    /// Set the fit/position mode for all monitors.
    pub fn set_fit(&self, fit: WallpaperFit) -> Result<()> {
        unsafe {
            self.desktop
                .SetPosition(fit.to_dwpos())
                .context("IDesktopWallpaper::SetPosition")
        }
    }
}

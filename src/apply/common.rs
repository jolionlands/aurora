use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MonitorSnapshot {
    pub monitor: MonitorInfo,
    pub current_path: Option<PathBuf>,
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

    pub fn as_str(self) -> &'static str {
        match self {
            WallpaperFit::Fill => "fill",
            WallpaperFit::Contain => "contain",
            WallpaperFit::Tile => "tile",
            WallpaperFit::Center => "center",
            WallpaperFit::Stretch => "stretch",
            WallpaperFit::Span => "span",
        }
    }
}

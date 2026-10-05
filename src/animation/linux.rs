//! Plasma renders animations inside its wallpaper item on Wayland and X11.
//! The daemon has no overlay windows or separate animation thread on Linux.
pub mod frames;
use crate::{apply::WallpaperFit, config::types::AnimatedConfig, transition::Rect};
pub use frames::may_be_animated;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct DisplayTarget {
    pub monitor_id: String,
    pub bounds: Rect,
    pub fit: WallpaperFit,
}

pub struct AnimationPlayer;

impl AnimationPlayer {
    pub fn from_config(_: &AnimatedConfig) -> Option<Self> {
        // The Linux applier configures the native Plasma animation player as
        // part of every commit. Runtime does not need a second player.
        None
    }
    pub fn show(&mut self, _: DisplayTarget, _: PathBuf) {}
    pub fn hide(&mut self, _: &str) {}
    pub fn hide_all(&mut self) {}
}

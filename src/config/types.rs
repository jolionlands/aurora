use std::path::PathBuf;

pub const DEFAULT_IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "ico",
];

/// Upper bound for a configured wallpaper transition.
pub const MAX_TRANSITION_DURATION_MS: u32 = 60_000;

#[derive(Debug, Clone)]
pub struct Config {
    pub sources: Vec<SourceConfig>,
    pub schedule: ScheduleConfig,
    pub monitors: Vec<MonitorOverride>,
    pub transitions: TransitionConfig,
    pub metrics: MetricsConfig,
    pub cache: CacheConfig,
    pub animated: AnimatedConfig,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            schedule: ScheduleConfig::default(),
            monitors: Vec::new(),
            transitions: TransitionConfig::default(),
            metrics: MetricsConfig::default(),
            cache: CacheConfig::default(),
            animated: AnimatedConfig::default(),
            log_level: "info".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub path: PathBuf,
    pub recursive: bool,
    pub extensions: Vec<String>,
    pub min_width: u32,
    pub min_height: u32,
    /// Skip portrait images (height > width), e.g. phone wallpapers.
    pub landscape_only: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            recursive: true,
            extensions: DEFAULT_IMAGE_EXTENSIONS
                .iter()
                .map(|extension| (*extension).to_string())
                .collect(),
            min_width: 1280,
            min_height: 720,
            landscape_only: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScheduleConfig {
    pub mode: String,
    pub interval_secs: u64,
    pub at_times: Vec<String>,
    pub on_workspace_change: bool,
    pub pause_when_fullscreen: bool,
    pub pause_when_idle_secs: u32,
    pub min_repeat_window: usize,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            mode: "interval".to_string(),
            interval_secs: 1800,
            at_times: Vec::new(),
            on_workspace_change: false,
            pause_when_fullscreen: true,
            pause_when_idle_secs: 0,
            min_repeat_window: 200,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MonitorOverride {
    pub name: String,
    pub fit: String,
}

impl Default for MonitorOverride {
    fn default() -> Self {
        Self {
            name: String::new(),
            fit: "fill".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransitionConfig {
    pub enabled: bool,
    pub duration_ms: u32,
    pub style: String,
    pub renderer: String,
}

impl Default for TransitionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: 800,
            style: "crossfade".to_string(),
            renderer: "auto".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub port: u16,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 9876,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub decoded_mb: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { decoded_mb: 256 }
    }
}

/// Upper bound for `animated.max-fps`.
pub const MAX_ANIMATION_FPS: u32 = 60;

/// Opt-in playback of animated GIF and WebP wallpapers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimatedConfig {
    pub enabled: bool,
    /// Frames shown faster than this rate are merged at decode time.
    pub max_fps: u32,
    /// Freeze animation while the laptop runs on battery power. Battery saver
    /// always freezes animation.
    pub pause_on_battery: bool,
    /// Freeze a display's animation while a fullscreen or maximized window
    /// covers it.
    pub pause_when_covered: bool,
    /// Decoded-frame budget per display; larger animations are downscaled.
    /// While playing, frames live on the GPU, which on integrated GPUs is
    /// system memory.
    pub max_memory_mb: u32,
    /// Frame budget per animation; longer animations drop evenly spaced frames.
    pub max_frames: u32,
    /// How frames are resampled: `auto` detects upscaled pixel art.
    pub scaling: AnimationScaling,
}

/// Resampling for animated wallpapers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnimationScaling {
    /// Pixel art (frames made of uniform NxN blocks) is stored at its native
    /// size and scaled by a whole factor with nearest-neighbour; everything
    /// else is filtered smoothly.
    #[default]
    Auto,
    Smooth,
    /// Always nearest-neighbour, at a whole factor when the art fits.
    Nearest,
}

impl AnimationScaling {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "smooth" | "linear" => Some(Self::Smooth),
            "nearest" | "pixel" | "pixel-art" => Some(Self::Nearest),
            _ => None,
        }
    }
}

impl Default for AnimatedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_fps: 15,
            pause_on_battery: true,
            pause_when_covered: true,
            max_memory_mb: 32,
            max_frames: 240,
            scaling: AnimationScaling::Auto,
        }
    }
}

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
            enabled: true,
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
    /// Deprecated compatibility field; parsed and warned about, never applied.
    pub deprecated_prefetch_count: Option<usize>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            decoded_mb: 256,
            deprecated_prefetch_count: None,
        }
    }
}

use anyhow::Result;

pub mod cpu;
pub mod gpu;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// A decoded image ready for blending — BGRA32 pixel data + dimensions.
///
/// TODO(audit): unify with crate::decode::DecodedImage when the foundation
/// wires all modules together.  The foundation uses field name `bgra`; we use
/// `data` here so callers can build a TransitionImage without depending on
/// decode internals.  A From<&crate::decode::DecodedImage> impl should be
/// added at integration time.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Raw BGRA bytes, length == width * height * 4.
    pub data: Vec<u8>,
}

/// Monitor screen-space rectangle.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionStyle {
    None,
    Crossfade,
    SlideLeft,
    SlideRight,
    WipeLeft,
    WipeRight,
    Dissolve,
    ZoomIn,
    ZoomOut,
}

impl TransitionStyle {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "crossfade" | "cross" => Self::Crossfade,
            "slideleft" | "slide_left" => Self::SlideLeft,
            "slideright" | "slide_right" => Self::SlideRight,
            "wipetleft" | "wipe_left" => Self::WipeLeft,
            "wiperight" | "wipe_right" => Self::WipeRight,
            "dissolve" => Self::Dissolve,
            "zoomin" | "zoom_in" => Self::ZoomIn,
            "zoomout" | "zoom_out" => Self::ZoomOut,
            _ => Self::None,
        }
    }

    /// Returns true for styles that the CPU renderer can handle in v1.
    pub fn cpu_capable(&self) -> bool {
        matches!(self, Self::Crossfade | Self::Dissolve | Self::None)
    }
}

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Gpu,
    Cpu,
    /// Try GPU; fall back to CPU if init fails.
    Auto,
}

impl Backend {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "gpu" => Self::Gpu,
            "cpu" => Self::Cpu,
            _ => Self::Auto,
        }
    }
}

// ---------------------------------------------------------------------------
// TransitionRenderer
// ---------------------------------------------------------------------------

pub struct TransitionRenderer {
    pub style: TransitionStyle,
    pub duration_ms: u32,
    resolved: ResolvedBackend,
}

enum ResolvedBackend {
    Gpu,
    Cpu,
}

impl TransitionRenderer {
    pub fn new(style: TransitionStyle, duration_ms: u32, backend: Backend) -> Self {
        let resolved = match backend {
            Backend::Gpu => ResolvedBackend::Gpu,
            Backend::Cpu => ResolvedBackend::Cpu,
            Backend::Auto => {
                // Try GPU; fall back to CPU
                if gpu::is_available() {
                    ResolvedBackend::Gpu
                } else {
                    ResolvedBackend::Cpu
                }
            }
        };

        // If style is not CPU-capable and we ended up with CPU, log a warning
        // but still proceed (CPU will do a simple crossfade approximation).
        if matches!(resolved, ResolvedBackend::Cpu) && !style.cpu_capable() {
            tracing::warn!(
                "style {:?} is not natively CPU-capable; approximating with crossfade",
                style
            );
        }

        Self {
            style,
            duration_ms,
            resolved,
        }
    }

    /// Run the full transition animation, blocking until completion.
    ///
    /// `monitor_bounds` — screen-space rect of the target monitor.
    /// `old` / `new` — BGRA32 decoded images.
    ///
    /// After this returns, the caller should commit the new wallpaper via
    /// `IDesktopWallpaper::SetWallpaper`.
    pub fn run(
        &self,
        monitor_bounds: Rect,
        old: &DecodedImage,
        new: &DecodedImage,
    ) -> Result<()> {
        if self.style == TransitionStyle::None || self.duration_ms == 0 {
            return Ok(());
        }

        match &self.resolved {
            ResolvedBackend::Gpu => {
                gpu::run_transition(monitor_bounds, old, new, &self.style, self.duration_ms)
            }
            ResolvedBackend::Cpu => {
                // For GPU-only styles, degrade gracefully to crossfade
                let effective_style = if self.style.cpu_capable() {
                    &self.style
                } else {
                    &TransitionStyle::Crossfade
                };
                cpu::run_transition(monitor_bounds, old, new, effective_style, self.duration_ms)
            }
        }
    }
}

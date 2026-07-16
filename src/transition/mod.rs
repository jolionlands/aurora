pub use crate::decode::DecodedImage;
use anyhow::Result;

pub mod cpu;
pub mod gpu;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// Monitor screen-space rectangle.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Resize an image to cover the monitor while preserving aspect ratio, then
/// crop the centered excess. This matches `IDesktopWallpaper`'s `fill` mode.
pub(crate) fn scale_to_cover(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let output_len = (dst_w as usize)
        .checked_mul(dst_h as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap_or(0);
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return vec![0; output_len];
    }

    let Some(source) = image::RgbaImage::from_raw(src_w, src_h, src.to_vec()) else {
        return vec![0; output_len];
    };
    let scale = (dst_w as f64 / src_w as f64).max(dst_h as f64 / src_h as f64);
    let scaled_w = ((src_w as f64 * scale).ceil() as u32).max(dst_w);
    let scaled_h = ((src_h as f64 * scale).ceil() as u32).max(dst_h);
    let scaled = image::imageops::resize(
        &source,
        scaled_w,
        scaled_h,
        image::imageops::FilterType::Triangle,
    );
    let left = (scaled_w - dst_w) / 2;
    let top = (scaled_h - dst_h) / 2;
    image::imageops::crop_imm(&scaled, left, top, dst_w, dst_h)
        .to_image()
        .into_raw()
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
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "crossfade" | "cross" => Self::Crossfade,
            "slideleft" | "slide_left" | "slide-left" => Self::SlideLeft,
            "slideright" | "slide_right" | "slide-right" => Self::SlideRight,
            "wipeleft" | "wipe_left" | "wipe-left" => Self::WipeLeft,
            "wiperight" | "wipe_right" | "wipe-right" => Self::WipeRight,
            "dissolve" => Self::Dissolve,
            "zoomin" | "zoom_in" | "zoom-in" => Self::ZoomIn,
            "zoomout" | "zoom_out" | "zoom-out" => Self::ZoomOut,
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
    pub fn parse(s: &str) -> Self {
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
    requested: Backend,
    resolved: ResolvedBackend,
}

enum ResolvedBackend {
    Gpu,
    Cpu,
}

impl TransitionRenderer {
    pub fn new(style: TransitionStyle, duration_ms: u32, backend: Backend) -> Self {
        let resolved = match &backend {
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
            requested: backend,
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
    pub fn run(&self, monitor_bounds: Rect, old: &DecodedImage, new: &DecodedImage) -> Result<()> {
        if self.style == TransitionStyle::None || self.duration_ms == 0 {
            return Ok(());
        }

        let run_cpu = || {
            // For GPU-only styles, degrade gracefully to crossfade
            let effective_style = if self.style.cpu_capable() {
                &self.style
            } else {
                &TransitionStyle::Crossfade
            };
            cpu::run_transition(monitor_bounds, old, new, effective_style, self.duration_ms)
        };

        run_selected_backend(
            &self.requested,
            &self.resolved,
            || gpu::run_transition(monitor_bounds, old, new, &self.style, self.duration_ms),
            run_cpu,
        )
    }
}

fn run_selected_backend<G, C>(
    requested: &Backend,
    resolved: &ResolvedBackend,
    run_gpu: G,
    run_cpu: C,
) -> Result<()>
where
    G: FnOnce() -> Result<()>,
    C: FnOnce() -> Result<()>,
{
    match resolved {
        ResolvedBackend::Gpu => match run_gpu() {
            Err(error) if *requested == Backend::Auto => {
                tracing::warn!(%error, "GPU transition failed; falling back to CPU");
                run_cpu()
            }
            result => result,
        },
        ResolvedBackend::Cpu => run_cpu(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        run_selected_backend, scale_to_cover, Backend, Rect, ResolvedBackend, TransitionRenderer,
        TransitionStyle,
    };
    use crate::decode::DecodedImage;
    use anyhow::anyhow;
    use std::cell::Cell;

    #[test]
    fn scale_to_cover_crops_without_stretching() {
        // 4x2 source covered into a 2x2 target: the left/right edges crop.
        let mut src = Vec::new();
        for _ in 0..2 {
            for x in 0..4u8 {
                src.extend_from_slice(&[x, 0, 0, 255]);
            }
        }
        let out = scale_to_cover(&src, 4, 2, 2, 2);
        assert_eq!(out.len(), 16);
        assert_eq!(out[0], out[8]);
        assert!(out[0] > 0 && out[0] < 3);
        assert!(out[4] > out[0]);
    }

    #[test]
    fn advertised_hyphenated_styles_parse() {
        for (name, expected) in [
            ("slide-left", TransitionStyle::SlideLeft),
            ("slide-right", TransitionStyle::SlideRight),
            ("wipe-left", TransitionStyle::WipeLeft),
            ("wipe-right", TransitionStyle::WipeRight),
            ("zoom-in", TransitionStyle::ZoomIn),
            ("zoom-out", TransitionStyle::ZoomOut),
        ] {
            assert_eq!(TransitionStyle::parse(name), expected);
        }
    }

    #[test]
    fn renderer_borrows_decoder_images_directly() {
        let image = DecodedImage {
            width: 1,
            height: 1,
            bgra: vec![0, 0, 0, 255],
        };
        TransitionRenderer::new(TransitionStyle::None, 0, Backend::Cpu)
            .run(
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &image,
                &image,
            )
            .unwrap();
    }

    #[test]
    fn auto_retries_cpu_after_gpu_failure() {
        let cpu_runs = Cell::new(0);

        run_selected_backend(
            &Backend::Auto,
            &ResolvedBackend::Gpu,
            || Err(anyhow!("GPU failed")),
            || {
                cpu_runs.set(cpu_runs.get() + 1);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(cpu_runs.get(), 1);
    }

    #[test]
    fn explicit_gpu_failure_is_returned_without_cpu_retry() {
        let cpu_runs = Cell::new(0);

        let error = run_selected_backend(
            &Backend::Gpu,
            &ResolvedBackend::Gpu,
            || Err(anyhow!("GPU failed")),
            || {
                cpu_runs.set(cpu_runs.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "GPU failed");
        assert_eq!(cpu_runs.get(), 0);
    }
}

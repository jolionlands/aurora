//! Bounded decoding of animated GIF and WebP files into display-ready frames.
//!
//! Frames are composited by the `image` crate, then downscaled (never
//! upscaled) to the smallest size that still covers the display. The result is
//! bounded three ways: frames shown faster than `max-fps` are merged, the
//! frame count is capped by merging evenly spaced neighbours, and the pixel
//! budget is enforced by shrinking every frame when it would be exceeded.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use image::imageops::FilterType;
use image::{AnimationDecoder, RgbaImage};

use crate::decode::{MAX_IMAGE_FILE_BYTES, MAX_IMAGE_PIXELS};

/// Browsers treat GIF delays of 10 ms or less as 100 ms; so do we, which keeps
/// "as fast as possible" GIFs from spinning the CPU.
const MIN_SOURCE_DELAY: Duration = Duration::from_millis(20);
const DEFAULT_FAST_DELAY: Duration = Duration::from_millis(100);
/// Stop reading pathological files after this many source frames.
const MAX_SOURCE_FRAMES: usize = 5_000;
/// Budget shrinking never goes below this edge length.
const MIN_EDGE: u32 = 96;

/// One display-ready frame: tightly packed BGRA rows plus its display time.
#[derive(Debug)]
pub struct Frame {
    pub bgra: Box<[u8]>,
    pub delay: Duration,
}

/// A decoded animation whose frames all share `width` x `height`.
#[derive(Debug)]
pub struct Animation {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<Frame>,
}

impl Animation {
    pub fn bytes(&self) -> usize {
        self.frames.iter().map(|frame| frame.bgra.len()).sum()
    }

    pub fn loop_duration(&self) -> Duration {
        self.frames.iter().map(|frame| frame.delay).sum()
    }
}

/// Limits applied while decoding one animation for one display.
#[derive(Debug, Clone, Copy)]
pub struct FrameBudget {
    /// Display size in physical pixels; frames never exceed what covers it.
    pub display_width: u32,
    pub display_height: u32,
    pub max_bytes: usize,
    pub max_frames: usize,
    pub min_delay: Duration,
}

/// Extensions whose files may contain more than one frame.
pub fn may_be_animated(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("gif") || extension.eq_ignore_ascii_case("webp")
        })
}

/// Decode `path` if it is an animated GIF or WebP. Returns `Ok(None)` for
/// single-frame files, which the normal static wallpaper already shows.
pub fn decode_animation(path: &Path, budget: &FrameBudget) -> Result<Option<Animation>> {
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !metadata.is_file() {
        bail!("animation path is not a file: {}", path.display());
    }
    if metadata.len() > MAX_IMAGE_FILE_BYTES {
        bail!(
            "animation {} is {} bytes; maximum is {MAX_IMAGE_FILE_BYTES}",
            path.display(),
            metadata.len()
        );
    }
    let (width, height) = image::image_dimensions(path)
        .with_context(|| format!("read dimensions of {}", path.display()))?;
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        bail!(
            "animation {} is {width}x{height}; too large",
            path.display()
        );
    }

    let reader =
        BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    let format = image::ImageFormat::from_path(path).ok();
    let frames = match format {
        Some(image::ImageFormat::Gif) => image::codecs::gif::GifDecoder::new(reader)
            .context("read GIF header")?
            .into_frames(),
        Some(image::ImageFormat::WebP) => {
            let decoder =
                image::codecs::webp::WebPDecoder::new(reader).context("read WebP header")?;
            if !decoder.has_animation() {
                return Ok(None);
            }
            decoder.into_frames()
        }
        _ => return Ok(None),
    };
    collect_frames(
        frames.map(|frame| {
            let frame = frame?;
            let delay = source_delay(frame.delay().numer_denom_ms());
            Ok((frame.into_buffer(), delay))
        }),
        budget,
    )
}

fn source_delay((numer, denom): (u32, u32)) -> Duration {
    let millis = numer.checked_div(denom).unwrap_or(0);
    let delay = Duration::from_millis(u64::from(millis));
    if delay < MIN_SOURCE_DELAY {
        DEFAULT_FAST_DELAY
    } else {
        delay
    }
}

/// Size that covers the display without upscaling the source.
pub fn cover_size(src_w: u32, src_h: u32, display_w: u32, display_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || display_w == 0 || display_h == 0 {
        return (src_w, src_h);
    }
    let scale =
        (f64::from(display_w) / f64::from(src_w)).max(f64::from(display_h) / f64::from(src_h));
    if scale >= 1.0 {
        return (src_w, src_h);
    }
    (
        ((f64::from(src_w) * scale).ceil() as u32).clamp(1, src_w),
        ((f64::from(src_h) * scale).ceil() as u32).clamp(1, src_h),
    )
}

/// Largest size with the same aspect ratio whose `frames` fit in `max_bytes`.
fn budget_size(width: u32, height: u32, frames: usize, max_bytes: usize) -> Option<(u32, u32)> {
    let needed = frame_bytes(width, height).saturating_mul(frames.max(1));
    if needed <= max_bytes {
        return Some((width, height));
    }
    let scale = (max_bytes as f64 / needed as f64).sqrt() * 0.98;
    let new_w = (f64::from(width) * scale).floor() as u32;
    let new_h = (f64::from(height) * scale).floor() as u32;
    (new_w.min(new_h) >= MIN_EDGE).then_some((new_w, new_h))
}

fn frame_bytes(width: u32, height: u32) -> usize {
    (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4)
}

fn to_bgra(image: &RgbaImage) -> Box<[u8]> {
    let mut bgra = image.as_raw().clone();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    bgra.into_boxed_slice()
}

fn resize_bgra(frame: &Frame, from: (u32, u32), to: (u32, u32)) -> Box<[u8]> {
    // Channel order does not matter to the resampler.
    let Some(image) = RgbaImage::from_raw(from.0, from.1, frame.bgra.to_vec()) else {
        return vec![0; frame_bytes(to.0, to.1)].into_boxed_slice();
    };
    image::imageops::resize(&image, to.0, to.1, FilterType::Triangle)
        .into_raw()
        .into_boxed_slice()
}

/// Merge each odd frame into its predecessor, halving the frame count while
/// keeping the loop duration.
fn halve_frames(frames: &mut Vec<Frame>) {
    let mut kept: Vec<Frame> = Vec::with_capacity(frames.len().div_ceil(2));
    for (index, frame) in frames.drain(..).enumerate() {
        match kept.last_mut() {
            Some(previous) if index % 2 == 1 => previous.delay += frame.delay,
            _ => kept.push(frame),
        }
    }
    *frames = kept;
}

/// Core of [`decode_animation`], separated so tests can feed synthetic frames.
pub(crate) fn collect_frames(
    source: impl Iterator<Item = image::ImageResult<(RgbaImage, Duration)>>,
    budget: &FrameBudget,
) -> Result<Option<Animation>> {
    let max_frames = budget.max_frames.max(2);
    let mut frames: Vec<Frame> = Vec::new();
    let mut size: Option<(u32, u32)> = None;
    // After halving the frame list, only every `stride`-th source frame is
    // kept so later frames keep the same spacing.
    let mut stride = 1usize;

    for (index, item) in source.take(MAX_SOURCE_FRAMES).enumerate() {
        let (image, delay) = item.context("decode animation frame")?;

        let target = *size.get_or_insert_with(|| {
            let (w, h) = cover_size(
                image.width(),
                image.height(),
                budget.display_width,
                budget.display_height,
            );
            // Always leave room for at least two frames at the chosen size.
            budget_size(w, h, 2, budget.max_bytes).unwrap_or((w.min(MIN_EDGE), h.min(MIN_EDGE)))
        });

        // Too-fast frames extend the previous frame instead of being stored.
        if let Some(previous) = frames.last_mut() {
            if previous.delay < budget.min_delay || !index.is_multiple_of(stride) {
                previous.delay += delay;
                continue;
            }
        }

        let bgra = if (image.width(), image.height()) == target {
            to_bgra(&image)
        } else {
            let resized = image::imageops::resize(&image, target.0, target.1, FilterType::Triangle);
            to_bgra(&resized)
        };
        frames.push(Frame { bgra, delay });

        if frames.len() > max_frames {
            halve_frames(&mut frames);
            stride = stride.saturating_mul(2);
        }
        let current = size.unwrap_or(target);
        if frame_bytes(current.0, current.1).saturating_mul(frames.len()) > budget.max_bytes {
            match budget_size(current.0, current.1, frames.len() * 2, budget.max_bytes) {
                // Shrink now with headroom for as many frames again.
                Some(smaller) => {
                    for frame in &mut frames {
                        frame.bgra = resize_bgra(frame, current, smaller);
                    }
                    size = Some(smaller);
                }
                // Already at the minimum size: drop frames instead.
                None => {
                    halve_frames(&mut frames);
                    stride = stride.saturating_mul(2);
                }
            }
        }
    }

    if frames.len() < 2 {
        return Ok(None);
    }
    let (width, height) = size.unwrap_or((0, 0));
    Ok(Some(Animation {
        width,
        height,
        frames,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(max_bytes: usize, max_frames: usize, min_delay_ms: u64) -> FrameBudget {
        FrameBudget {
            display_width: 1280,
            display_height: 800,
            max_bytes,
            max_frames,
            min_delay: Duration::from_millis(min_delay_ms),
        }
    }

    fn synthetic(
        count: usize,
        width: u32,
        height: u32,
        delay_ms: u64,
    ) -> impl Iterator<Item = image::ImageResult<(RgbaImage, Duration)>> {
        (0..count).map(move |index| {
            let shade = (index % 256) as u8;
            Ok((
                RgbaImage::from_pixel(width, height, image::Rgba([shade, 0, 255 - shade, 255])),
                Duration::from_millis(delay_ms),
            ))
        })
    }

    #[test]
    fn single_frame_is_not_an_animation() {
        let result = collect_frames(synthetic(1, 64, 64, 100), &budget(1 << 30, 100, 0)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn small_animation_is_kept_at_source_size_in_bgra() {
        let animation = collect_frames(synthetic(3, 320, 200, 80), &budget(1 << 30, 100, 0))
            .unwrap()
            .unwrap();
        assert_eq!((animation.width, animation.height), (320, 200));
        assert_eq!(animation.frames.len(), 3);
        // Frame 0 is rgba(0,0,255): stored as BGRA it starts with blue.
        assert_eq!(&animation.frames[0].bgra[..4], &[255, 0, 0, 255]);
        assert_eq!(animation.loop_duration(), Duration::from_millis(240));
    }

    #[test]
    fn oversized_source_is_downscaled_to_cover_the_display() {
        assert_eq!(cover_size(3840, 2160, 1280, 800), (1423, 800));
        assert_eq!(cover_size(640, 360, 1280, 800), (640, 360));
        assert_eq!(cover_size(0, 10, 1280, 800), (0, 10));
    }

    #[test]
    fn frames_faster_than_max_fps_are_merged_without_losing_time() {
        // 30 fps source, 15 fps cap: every other frame is merged.
        let animation = collect_frames(synthetic(10, 64, 64, 33), &budget(1 << 30, 100, 66))
            .unwrap()
            .unwrap();
        assert_eq!(animation.frames.len(), 5);
        assert_eq!(animation.loop_duration(), Duration::from_millis(330));
        assert!(animation
            .frames
            .iter()
            .all(|frame| frame.delay >= Duration::from_millis(66)));
    }

    #[test]
    fn frame_cap_halves_evenly_and_keeps_loop_duration() {
        let animation = collect_frames(synthetic(100, 32, 32, 50), &budget(1 << 30, 30, 0))
            .unwrap()
            .unwrap();
        assert!(animation.frames.len() <= 30, "{}", animation.frames.len());
        assert!(animation.frames.len() >= 13);
        assert_eq!(animation.loop_duration(), Duration::from_millis(5000));
    }

    #[test]
    fn memory_budget_shrinks_frames_and_is_respected() {
        let max_bytes = 4 * 1024 * 1024;
        let animation = collect_frames(synthetic(60, 800, 500, 100), &budget(max_bytes, 240, 0))
            .unwrap()
            .unwrap();
        assert!(
            animation.bytes() <= max_bytes,
            "{} bytes",
            animation.bytes()
        );
        assert!(animation.width < 800 && animation.width >= MIN_EDGE);
        let ratio = f64::from(animation.width) / f64::from(animation.height);
        assert!((ratio - 1.6).abs() < 0.05, "aspect drifted to {ratio}");
        assert_eq!(animation.frames.len(), 60);
        assert_eq!(animation.loop_duration(), Duration::from_millis(6000));
        let expected = (animation.width * animation.height * 4) as usize;
        assert!(animation
            .frames
            .iter()
            .all(|frame| frame.bgra.len() == expected));
    }

    #[test]
    fn memory_budget_drops_frames_once_frames_are_minimal() {
        let max_bytes = 1024 * 1024;
        let animation = collect_frames(synthetic(400, 200, 200, 40), &budget(max_bytes, 2000, 0))
            .unwrap()
            .unwrap();
        assert!(
            animation.bytes() <= max_bytes,
            "{} bytes",
            animation.bytes()
        );
        assert!(animation.width >= MIN_EDGE);
        assert_eq!(animation.loop_duration(), Duration::from_millis(16_000));
    }

    #[test]
    fn tiny_gif_delays_use_the_browser_default() {
        assert_eq!(source_delay((0, 1)), DEFAULT_FAST_DELAY);
        assert_eq!(source_delay((10, 1)), DEFAULT_FAST_DELAY);
        assert_eq!(source_delay((70, 1)), Duration::from_millis(70));
        assert_eq!(source_delay((5, 0)), DEFAULT_FAST_DELAY);
    }

    #[test]
    fn decodes_real_gif_and_rejects_static_gif() {
        use image::codecs::gif::{GifEncoder, Repeat};
        let dir = tempfile::tempdir().unwrap();

        let animated = dir.path().join("spin.gif");
        {
            let file = File::create(&animated).unwrap();
            let mut encoder = GifEncoder::new(file);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for (image, delay) in synthetic(4, 48, 32, 120).map(Result::unwrap) {
                let delay = image::Delay::from_saturating_duration(delay);
                encoder
                    .encode_frame(image::Frame::from_parts(image, 0, 0, delay))
                    .unwrap();
            }
        }
        let decoded = decode_animation(&animated, &budget(1 << 30, 100, 0))
            .unwrap()
            .unwrap();
        assert_eq!(decoded.frames.len(), 4);
        assert_eq!((decoded.width, decoded.height), (48, 32));
        assert_eq!(decoded.loop_duration(), Duration::from_millis(480));

        let still = dir.path().join("still.gif");
        RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]))
            .save(&still)
            .unwrap();
        assert!(decode_animation(&still, &budget(1 << 30, 100, 0))
            .unwrap()
            .is_none());
        assert!(may_be_animated(&still));
        assert!(may_be_animated(Path::new("x.WEBP")));
        assert!(!may_be_animated(Path::new("x.png")));
    }
}

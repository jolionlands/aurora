//! Bounded decoding of animated GIF and WebP files into display-ready frames.
//!
//! GIFs are decoded with the `gif` crate as palette indices and composited
//! into one persistent canvas (disposal and transparency handled here), so
//! memory stays near one canvas plus the stored output frames. WebP goes
//! through the `image` crate's compositor.
//!
//! Every composited frame is downscaled once (never upscaled) with an integer
//! area filter to the smallest size that covers the display and fits the
//! memory budget. The result is bounded three ways: frames shown faster than
//! `max-fps` or identical to their predecessor are merged, the frame count is
//! capped by merging evenly spaced neighbours, and the byte budget is enforced
//! by choosing the size up front (from a cheap GIF metadata pass) with an
//! adaptive shrink as a safety net.

use std::fs::File;
use std::io::BufReader;
use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
#[cfg(test)]
use image::RgbaImage;
use image::{AnimationDecoder, ImageBuffer, Rgba};

use crate::decode::{MAX_IMAGE_FILE_BYTES, MAX_IMAGE_PIXELS};

/// Browsers treat GIF delays of 10 ms or less as 100 ms; so do we, which keeps
/// "as fast as possible" GIFs from spinning the CPU.
const MIN_SOURCE_DELAY: Duration = Duration::from_millis(20);
const DEFAULT_FAST_DELAY: Duration = Duration::from_millis(100);
/// Stop reading pathological files after this many source frames.
const MAX_SOURCE_FRAMES: usize = 5_000;
/// Smallest native size (per side) treated as upscaled pixel art.
const MIN_ART_EDGE: u32 = 32;
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
    /// Scale with nearest-neighbour at whole factors (pixel art).
    pub pixel_art: bool,
    /// Opaque BGRA fill for letterbox bars and transparent pixels, sampled
    /// from the first frame's border.
    pub background: [u8; 4],
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
    pub scaling: crate::config::types::AnimationScaling,
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
    match image::ImageFormat::from_path(path).ok() {
        Some(image::ImageFormat::Gif) => decode_gif(path, budget),
        Some(image::ImageFormat::WebP) => decode_webp(path, budget),
        _ => Ok(None),
    }
    .with_context(|| format!("decode animation {}", path.display()))
}

fn open(path: &Path) -> Result<BufReader<File>> {
    Ok(BufReader::new(
        File::open(path).with_context(|| format!("open {}", path.display()))?,
    ))
}

fn check_canvas(width: u32, height: u32) -> Result<()> {
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        bail!("canvas {width}x{height} is too large");
    }
    if width == 0 || height == 0 {
        bail!("canvas has no area");
    }
    Ok(())
}

fn gif_options() -> gif::DecodeOptions {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::Indexed);
    // Indexed output is one byte per pixel.
    if let Some(limit) = NonZeroU64::new(MAX_IMAGE_PIXELS) {
        options.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    }
    options
}

fn gif_delay(centiseconds: u16) -> Duration {
    source_delay((u32::from(centiseconds) * 10, 1))
}

/// Frame delays from GIF block headers only (no pixel decoding), used to
/// size the output before any frame is decoded.
fn gif_delays(path: &Path) -> Result<Vec<Duration>> {
    let mut decoder = gif_options()
        .read_info(open(path)?)
        .context("read GIF header")?;
    let mut delays = Vec::new();
    while let Some(frame) = decoder.next_frame_info().context("read GIF frame header")? {
        delays.push(gif_delay(frame.delay));
        if delays.len() >= MAX_SOURCE_FRAMES {
            break;
        }
    }
    Ok(delays)
}

fn decode_gif(path: &Path, budget: &FrameBudget) -> Result<Option<Animation>> {
    let delays = gif_delays(path)?;
    if delays.len() < 2 {
        return Ok(None);
    }
    let decoder = gif_options()
        .read_info(open(path)?)
        .context("read GIF header")?;
    check_canvas(u32::from(decoder.width()), u32::from(decoder.height()))?;
    drop(decoder);
    let planned = planned_frames(&delays, budget.min_delay);
    match decode_gif_frames(path, budget, planned, None)? {
        GifPass::Done(animation) => Ok(animation),
        // A later frame broke the block pattern: decode again without it.
        GifPass::NotPixelArt => match decode_gif_frames(path, budget, planned, Some(1))? {
            GifPass::Done(animation) => Ok(animation),
            GifPass::NotPixelArt => bail!("pixel-art detection did not settle"),
        },
    }
}

enum GifPass {
    Done(Option<Animation>),
    NotPixelArt,
}

/// One full decode. `block` forces a block size; `None` detects it from the
/// first frame (subject to the configured scaling).
fn decode_gif_frames(
    path: &Path,
    budget: &FrameBudget,
    planned: usize,
    block: Option<u32>,
) -> Result<GifPass> {
    use crate::config::types::AnimationScaling;

    let mut decoder = gif_options()
        .read_info(open(path)?)
        .context("read GIF header")?;
    let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
    let global_palette = decoder.global_palette().map(<[u8]>::to_vec);
    let mut canvas = GifCanvas::new(width as usize, height as usize);
    let mut collector = FrameCollector::new(budget, Some(planned));
    let mut block = block;

    while let Some(frame) = decoder.read_next_frame().context("decode GIF frame")? {
        let palette = frame
            .palette
            .as_deref()
            .or(global_palette.as_deref())
            .context("GIF frame has no color table")?;
        canvas.draw(frame, palette);
        let size = *block.get_or_insert_with(|| match budget.scaling {
            AnimationScaling::Smooth => 1,
            AnimationScaling::Auto | AnimationScaling::Nearest => {
                pixel_block(&canvas.rgba, width, height)
            }
        });
        if size > 1 && pixel_block_of(&canvas.rgba, width, height, size) != size {
            return Ok(GifPass::NotPixelArt);
        }
        collector.set_pixel_block(size);
        collector.push(&canvas.rgba, width, height, gif_delay(frame.delay))?;
        canvas.dispose(frame);
        if collector.is_full() {
            break;
        }
    }
    Ok(GifPass::Done(collector.finish()))
}

/// Largest block size (up to 16) such that the image consists of uniform
/// `n`x`n` squares aligned to the origin; 1 when it is not upscaled art.
pub(crate) fn pixel_block(rgba: &[u8], width: u32, height: u32) -> u32 {
    (2..=16u32)
        .rev()
        .find(|&n| pixel_block_of(rgba, width, height, n) == n)
        .unwrap_or(1)
}

/// `n` if every `n`x`n` block of the image is uniform, else 1.
fn pixel_block_of(rgba: &[u8], width: u32, height: u32, n: u32) -> u32 {
    // The art itself must be at least MIN_ART_EDGE on each side; smaller
    // "art" is more likely a flat or tiny image than upscaled pixel art.
    if n < 2
        || !width.is_multiple_of(n)
        || !height.is_multiple_of(n)
        || width / n < MIN_ART_EDGE
        || height / n < MIN_ART_EDGE
    {
        return 1;
    }
    let (w, n_us) = (width as usize, n as usize);
    let pixel = |x: usize, y: usize| &rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
    for y in 0..height as usize {
        let row_anchor = y - y % n_us;
        for x in 0..w {
            let anchor = pixel(x - x % n_us, row_anchor);
            if pixel(x, y) != anchor {
                return 1;
            }
        }
    }
    n
}

fn decode_webp(path: &Path, budget: &FrameBudget) -> Result<Option<Animation>> {
    let decoder = image::codecs::webp::WebPDecoder::new(open(path)?).context("read WebP header")?;
    if !decoder.has_animation() {
        return Ok(None);
    }
    let mut collector = FrameCollector::new(budget, None);
    for frame in decoder.into_frames() {
        let frame = frame.context("decode WebP frame")?;
        let delay = source_delay(frame.delay().numer_denom_ms());
        let buffer = frame.into_buffer();
        check_canvas(buffer.width(), buffer.height())?;
        collector.push(buffer.as_raw(), buffer.width(), buffer.height(), delay)?;
        if collector.is_full() {
            break;
        }
    }
    Ok(collector.finish())
}

/// The GIF logical screen, composited frame by frame.
struct GifCanvas {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    /// Pixels under the current frame, kept for `DisposalMethod::Previous`.
    saved: Vec<u8>,
}

impl GifCanvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; width * height * 4],
            saved: Vec::new(),
        }
    }

    /// The frame rectangle clipped to the canvas: (left, top, width, height).
    fn clip(&self, frame: &gif::Frame<'_>) -> (usize, usize, usize, usize) {
        let left = usize::from(frame.left).min(self.width);
        let top = usize::from(frame.top).min(self.height);
        let width = usize::from(frame.width).min(self.width - left);
        let height = usize::from(frame.height).min(self.height - top);
        (left, top, width, height)
    }

    fn draw(&mut self, frame: &gif::Frame<'_>, palette: &[u8]) {
        let (left, top, width, height) = self.clip(frame);
        if frame.dispose == gif::DisposalMethod::Previous {
            self.saved.clear();
            for row in top..top + height {
                let start = (row * self.width + left) * 4;
                self.saved
                    .extend_from_slice(&self.rgba[start..start + width * 4]);
            }
        }
        let source_width = usize::from(frame.width);
        for row in 0..height {
            let source = &frame.buffer[row * source_width..][..width];
            let start = ((top + row) * self.width + left) * 4;
            let target = &mut self.rgba[start..start + width * 4];
            for (pixel, &index) in target.chunks_exact_mut(4).zip(source) {
                if Some(index) == frame.transparent {
                    continue;
                }
                let color = usize::from(index) * 3;
                if let Some(rgb) = palette.get(color..color + 3) {
                    pixel.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
            }
        }
    }

    fn dispose(&mut self, frame: &gif::Frame<'_>) {
        let (left, top, width, height) = self.clip(frame);
        match frame.dispose {
            gif::DisposalMethod::Background => {
                for row in top..top + height {
                    let start = (row * self.width + left) * 4;
                    self.rgba[start..start + width * 4].fill(0);
                }
            }
            gif::DisposalMethod::Previous if self.saved.len() == width * height * 4 => {
                for (index, row) in (top..top + height).enumerate() {
                    let start = (row * self.width + left) * 4;
                    self.rgba[start..start + width * 4]
                        .copy_from_slice(&self.saved[index * width * 4..][..width * 4]);
                }
            }
            _ => {}
        }
    }
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

/// How many frames survive `min_delay` merging (the collector's rule).
fn planned_frames(delays: &[Duration], min_delay: Duration) -> usize {
    let mut kept = 0usize;
    let mut last = Duration::MAX;
    for &delay in delays {
        if kept > 0 && last < min_delay {
            last += delay;
        } else {
            kept += 1;
            last = delay;
        }
    }
    kept
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

/// Nearest-neighbour resample (keeps pixel art crisp).
fn nearest(rgba: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let (fw, fh) = (from.0 as usize, from.1 as usize);
    let (tw, th) = (to.0 as usize, to.1 as usize);
    let mut out = Vec::with_capacity(tw * th * 4);
    for y in 0..th {
        let sy = (y * fh / th.max(1)).min(fh.saturating_sub(1));
        for x in 0..tw {
            let sx = (x * fw / tw.max(1)).min(fw.saturating_sub(1));
            let at = (sy * fw + sx) * 4;
            out.extend_from_slice(&rgba[at..at + 4]);
        }
    }
    out
}

/// Downscale (integer area filter, or nearest for pixel art) or copy `rgba`,
/// returning BGRA.
fn scaled_bgra(rgba: &[u8], from: (u32, u32), to: (u32, u32), pixel_art: bool) -> Box<[u8]> {
    let mut out = if from == to {
        rgba.to_vec()
    } else if pixel_art {
        nearest(rgba, from, to)
    } else {
        match ImageBuffer::<Rgba<u8>, &[u8]>::from_raw(from.0, from.1, rgba) {
            Some(view) => image::imageops::thumbnail(&view, to.0, to.1).into_raw(),
            None => vec![0; frame_bytes(to.0, to.1)],
        }
    };
    for pixel in out.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    out.into_boxed_slice()
}

/// Rescale an already stored BGRA frame (channel order is irrelevant).
fn rescale_stored(frame: &Frame, from: (u32, u32), to: (u32, u32), pixel_art: bool) -> Box<[u8]> {
    if pixel_art {
        return nearest(&frame.bgra, from, to).into_boxed_slice();
    }
    match ImageBuffer::<Rgba<u8>, &[u8]>::from_raw(from.0, from.1, &frame.bgra) {
        Some(view) => image::imageops::thumbnail(&view, to.0, to.1)
            .into_raw()
            .into_boxed_slice(),
        None => vec![0; frame_bytes(to.0, to.1)].into_boxed_slice(),
    }
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

/// Turns a stream of composited RGBA canvases into a bounded [`Animation`].
pub(crate) struct FrameCollector<'a> {
    budget: &'a FrameBudget,
    max_frames: usize,
    /// Frame count expected after rate merging, when known in advance.
    planned: Option<usize>,
    frames: Vec<Frame>,
    size: Option<(u32, u32)>,
    /// After halving the frame list, only every `stride`-th source frame is
    /// kept so later frames keep the same spacing.
    stride: usize,
    seen: usize,
    /// Source pixels per art pixel; >1 stores frames at native art size
    /// with nearest-neighbour sampling.
    pixel_block: u32,
}

impl<'a> FrameCollector<'a> {
    pub(crate) fn new(budget: &'a FrameBudget, planned: Option<usize>) -> Self {
        Self {
            budget,
            max_frames: budget.max_frames.max(2),
            planned,
            frames: Vec::new(),
            size: None,
            stride: 1,
            seen: 0,
            pixel_block: 1,
        }
    }

    fn set_pixel_block(&mut self, block: u32) {
        if self.frames.is_empty() {
            self.pixel_block = block.max(1);
        }
    }

    fn pixel_art(&self) -> bool {
        self.pixel_block > 1
            || self.budget.scaling == crate::config::types::AnimationScaling::Nearest
    }

    fn is_full(&self) -> bool {
        self.seen >= MAX_SOURCE_FRAMES
    }

    fn target_size(&mut self, width: u32, height: u32) -> (u32, u32) {
        let block = self.pixel_block;
        *self.size.get_or_insert_with(|| {
            let (w, h) = if block > 1 {
                // Native art size; the GPU scales it up by a whole factor.
                (width / block, height / block)
            } else {
                cover_size(
                    width,
                    height,
                    self.budget.display_width,
                    self.budget.display_height,
                )
            };
            let frames = self
                .planned
                .map_or(2, |planned| planned.min(self.max_frames))
                .max(2);
            budget_size(w, h, frames, self.budget.max_bytes)
                .or_else(|| budget_size(w, h, 2, self.budget.max_bytes))
                .unwrap_or((w.min(MIN_EDGE), h.min(MIN_EDGE)))
        })
    }

    /// Offer the next composited canvas (`width` x `height` RGBA).
    pub(crate) fn push(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        delay: Duration,
    ) -> Result<()> {
        let index = self.seen;
        self.seen += 1;
        if rgba.len() != frame_bytes(width, height) {
            bail!("frame buffer does not match {width}x{height}");
        }
        let target = self.target_size(width, height);

        // Too-fast frames extend the previous frame instead of being stored.
        if let Some(previous) = self.frames.last_mut() {
            if previous.delay < self.budget.min_delay || !index.is_multiple_of(self.stride) {
                previous.delay += delay;
                return Ok(());
            }
        }

        let bgra = scaled_bgra(rgba, (width, height), target, self.pixel_art());
        // Identical frames (common in GIFs that hold a pose) cost nothing.
        if let Some(previous) = self.frames.last_mut() {
            if previous.bgra == bgra {
                previous.delay += delay;
                return Ok(());
            }
        }
        self.frames.push(Frame { bgra, delay });

        if self.frames.len() > self.max_frames {
            halve_frames(&mut self.frames);
            self.stride = self.stride.saturating_mul(2);
        }
        let current = self.size.unwrap_or(target);
        if frame_bytes(current.0, current.1).saturating_mul(self.frames.len())
            > self.budget.max_bytes
        {
            match budget_size(
                current.0,
                current.1,
                self.frames.len() * 2,
                self.budget.max_bytes,
            ) {
                // Shrink now with headroom for as many frames again.
                Some(smaller) => {
                    let pixel_art = self.pixel_art();
                    for frame in &mut self.frames {
                        frame.bgra = rescale_stored(frame, current, smaller, pixel_art);
                    }
                    self.size = Some(smaller);
                }
                // Already at the minimum size: drop frames instead.
                None => {
                    halve_frames(&mut self.frames);
                    self.stride = self.stride.saturating_mul(2);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Option<Animation> {
        if self.frames.len() < 2 {
            return None;
        }
        let pixel_art = self.pixel_art();
        let (width, height) = self.size.unwrap_or((0, 0));
        let mut frames = self.frames;
        let background = border_color(&frames[0].bgra, width, height);
        // Transparent pixels would otherwise show as black.
        for frame in &mut frames {
            for pixel in frame.bgra.chunks_exact_mut(4) {
                if pixel[3] == 0 {
                    pixel.copy_from_slice(&background);
                } else {
                    pixel[3] = 255;
                }
            }
        }
        Some(Animation {
            width,
            height,
            frames,
            pixel_art,
            background,
        })
    }
}

/// Most common opaque colour on the image border (BGRA), black if none.
fn border_color(bgra: &[u8], width: u32, height: u32) -> [u8; 4] {
    use std::collections::HashMap;
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || bgra.len() < w * h * 4 {
        return [0, 0, 0, 255];
    }
    let mut counts: HashMap<[u8; 3], usize> = HashMap::new();
    let mut count = |x: usize, y: usize| {
        let at = (y * w + x) * 4;
        if bgra[at + 3] != 0 {
            *counts
                .entry([bgra[at], bgra[at + 1], bgra[at + 2]])
                .or_default() += 1;
        }
    };
    for x in 0..w {
        count(x, 0);
        count(x, h - 1);
    }
    for y in 0..h {
        count(0, y);
        count(w - 1, y);
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map_or([0, 0, 0, 255], |(c, _)| [c[0], c[1], c[2], 255])
}

/// Collect an iterator of RGBA frames (used by tests).
#[cfg(test)]
pub(crate) fn collect_frames(
    source: impl Iterator<Item = image::ImageResult<(RgbaImage, Duration)>>,
    budget: &FrameBudget,
) -> Result<Option<Animation>> {
    let mut collector = FrameCollector::new(budget, None);
    for item in source {
        let (image, delay) = item.context("decode animation frame")?;
        collector.push(image.as_raw(), image.width(), image.height(), delay)?;
    }
    Ok(collector.finish())
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
            scaling: crate::config::types::AnimationScaling::Auto,
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

    /// A 4x4 GIF exercising sub-rectangles, transparency, and every disposal.
    fn disposal_gif(path: &Path) {
        use gif::{DisposalMethod, Encoder, Frame as GifFrame, Repeat};
        // 0 red, 1 green, 2 blue, 3 white (transparent in overlays)
        let palette = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let mut encoder = Encoder::new(File::create(path).unwrap(), 4, 4, &palette).unwrap();
        encoder.set_repeat(Repeat::Infinite).unwrap();
        let mut write = |left, top, w, h, pixels: Vec<u8>, dispose, transparent| {
            let mut frame = GifFrame::from_indexed_pixels(w, h, pixels, transparent);
            frame.left = left;
            frame.top = top;
            frame.delay = 10;
            frame.dispose = dispose;
            encoder.write_frame(&frame).unwrap();
        };
        // Full red background, kept.
        write(0, 0, 4, 4, vec![0; 16], DisposalMethod::Keep, None);
        // Green 2x2 with a transparent corner, restored to previous after.
        write(
            1,
            1,
            2,
            2,
            vec![1, 1, 1, 3],
            DisposalMethod::Previous,
            Some(3),
        );
        // Blue 2x2 at the origin, cleared to background after.
        write(0, 0, 2, 2, vec![2; 4], DisposalMethod::Background, None);
        // Green column overlapping the cleared area, kept.
        write(1, 0, 1, 4, vec![1; 4], DisposalMethod::Keep, None);
    }

    #[test]
    fn native_gif_compositing_matches_the_image_crate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disposal.gif");
        disposal_gif(&path);

        let ours = decode_animation(&path, &budget(1 << 30, 100, 0))
            .unwrap()
            .unwrap();
        let reference: Vec<RgbaImage> =
            image::codecs::gif::GifDecoder::new(BufReader::new(File::open(&path).unwrap()))
                .unwrap()
                .into_frames()
                .map(|frame| frame.unwrap().into_buffer())
                .collect();
        assert_eq!(reference.len(), 4);
        assert_eq!(ours.frames.len(), 4);
        for (index, (frame, expected)) in ours.frames.iter().zip(&reference).enumerate() {
            let mut rgba = frame.bgra.to_vec();
            for pixel in rgba.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            // Transparent pixels are filled with the background colour; only
            // the opaque ones must match exactly.
            let background = [
                ours.background[2],
                ours.background[1],
                ours.background[0],
                255,
            ];
            for (a, b) in rgba.chunks_exact(4).zip(expected.as_raw().chunks_exact(4)) {
                if b[3] == 0 {
                    assert_eq!(a, background, "frame {index} transparent fill");
                } else {
                    assert_eq!(a, b, "frame {index} color");
                }
            }
        }
    }

    #[test]
    fn identical_frames_merge_and_malformed_rects_are_clipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hold.gif");
        {
            use gif::{Encoder, Frame as GifFrame, Repeat};
            let palette = [0, 0, 0, 255, 255, 255];
            let mut encoder = Encoder::new(File::create(&path).unwrap(), 4, 4, &palette).unwrap();
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for (index, color) in [0u8, 0, 0, 1].into_iter().enumerate() {
                let mut frame = GifFrame::from_indexed_pixels(4, 4, vec![color; 16], None);
                frame.delay = 10 + index as u16;
                encoder.write_frame(&frame).unwrap();
            }
            // A frame hanging off the canvas must not panic.
            let mut frame = GifFrame::from_indexed_pixels(4, 4, vec![0; 16], None);
            frame.left = 2;
            frame.top = 3;
            frame.delay = 10;
            encoder.write_frame(&frame).unwrap();
        }
        let animation = decode_animation(&path, &budget(1 << 30, 100, 0))
            .unwrap()
            .unwrap();
        // Three identical black frames collapse into one 330 ms frame.
        assert_eq!(animation.frames.len(), 3);
        assert_eq!(animation.frames[0].delay, Duration::from_millis(330));
    }

    #[test]
    fn upscaled_pixel_art_is_stored_at_native_size_with_nearest_sampling() {
        use gif::{Encoder, Frame as GifFrame, Repeat};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pixel.gif");
        // 32x32 art upscaled 4x to 128x128; index 2 is transparent.
        let palette = [10, 20, 30, 200, 50, 50, 0, 0, 0];
        {
            let mut encoder =
                Encoder::new(File::create(&path).unwrap(), 128, 128, &palette).unwrap();
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for shift in 0..3u16 {
                let pixels: Vec<u8> = (0..128u16 * 128)
                    .map(|i| {
                        let (x, y) = (i % 128 / 4, i / 128 / 4);
                        if x == 0 && y == 0 {
                            2
                        } else if (x + y + shift) % 5 == 0 {
                            1
                        } else {
                            0
                        }
                    })
                    .map(|v| v as u8)
                    .collect();
                let mut frame = GifFrame::from_indexed_pixels(128, 128, pixels, Some(2));
                frame.delay = 10;
                encoder.write_frame(&frame).unwrap();
            }
        }
        let animation = decode_animation(&path, &budget(1 << 30, 100, 0))
            .unwrap()
            .unwrap();
        assert!(animation.pixel_art);
        assert_eq!((animation.width, animation.height), (32, 32));
        assert_eq!(animation.frames.len(), 3);
        // Border is mostly palette 0 (rgb 10,20,30) -> BGRA background.
        assert_eq!(animation.background, [30, 20, 10, 255]);
        // The transparent corner is filled with the background.
        assert_eq!(&animation.frames[0].bgra[..4], &[30, 20, 10, 255]);
        // Every stored pixel is exactly a palette colour (no blending).
        for frame in &animation.frames {
            for pixel in frame.bgra.chunks_exact(4) {
                assert!(
                    pixel == [30, 20, 10, 255] || pixel == [50, 50, 200, 255],
                    "{pixel:?}"
                );
            }
        }

        let smooth = FrameBudget {
            scaling: crate::config::types::AnimationScaling::Smooth,
            ..budget(1 << 30, 100, 0)
        };
        let animation = decode_animation(&path, &smooth).unwrap().unwrap();
        assert!(!animation.pixel_art);
        assert_eq!((animation.width, animation.height), (128, 128));
    }

    #[test]
    fn block_detection_rejects_photos_and_small_images() {
        let mut rgba = vec![0u8; 64 * 64 * 4];
        // Flat images are trivially blocky; the native-size floor caps it.
        assert_eq!(pixel_block(&rgba, 64, 64), 2);
        rgba[4 * 5] = 9; // one odd pixel at x=5
        assert_eq!(pixel_block(&rgba, 64, 64), 1);
        assert_eq!(pixel_block(&[0; 16 * 16 * 4], 16, 16), 1);
    }

    #[test]
    fn planned_frames_follows_the_merge_rule() {
        let ms = Duration::from_millis;
        assert_eq!(planned_frames(&[ms(33); 10], ms(66)), 5);
        assert_eq!(planned_frames(&[ms(100); 7], ms(66)), 7);
        assert_eq!(planned_frames(&[], ms(66)), 0);
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

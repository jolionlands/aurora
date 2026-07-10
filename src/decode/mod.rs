use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Decoded image
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Raw pixel data in BGRA byte order, tightly packed (stride = width * 4).
    pub bgra: Vec<u8>,
}

/// Refuse inputs that would make the image decoder or autotag payload
/// unbounded before reading the full file into memory.
pub const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 100_000_000;

fn validate_file_size(path: &Path) -> Result<()> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("metadata for image {:?}", path))?;
    if !metadata.is_file() {
        bail!("image path is not a file: {:?}", path);
    }
    if metadata.len() > MAX_IMAGE_FILE_BYTES {
        bail!(
            "image file {:?} is {} bytes; maximum is {} bytes",
            path,
            metadata.len(),
            MAX_IMAGE_FILE_BYTES
        );
    }
    Ok(())
}

fn validate_pixel_dimensions(width: u32, height: u32) -> Result<()> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_IMAGE_PIXELS {
        bail!(
            "image dimensions {}x{} ({} pixels) exceed maximum of {} pixels",
            width,
            height,
            pixels,
            MAX_IMAGE_PIXELS
        );
    }
    Ok(())
}

/// Validate an image before callers read or decode its full contents.
pub fn validate_image_file(path: &Path) -> Result<(u32, u32)> {
    validate_file_size(path)?;
    let dimensions = image::image_dimensions(path)
        .with_context(|| format!("read image dimensions for {:?}", path))?;
    validate_pixel_dimensions(dimensions.0, dimensions.1)?;
    Ok(dimensions)
}

// ---------------------------------------------------------------------------
// Decode pipeline
// ---------------------------------------------------------------------------

/// Decode `path` to a BGRA buffer scaled to fit within `(target_w, target_h)`
/// while preserving aspect ratio.  Never upscales.
///
/// Primary path: `image` crate (JPEG, PNG, GIF, WebP, AVIF, TIFF, BMP, ICO).
/// Fallback path: Windows Imaging Component (WIC) for formats like HEIC that
/// the `image` crate does not support.
pub fn decode_image(path: &Path, target_w: u32, target_h: u32) -> Result<DecodedImage> {
    validate_file_size(path)?;
    // `image_dimensions` reads only the format header. If this format is
    // WIC-only, the fallback validates dimensions after opening its frame.
    if let Ok((src_w, src_h)) = image::image_dimensions(path) {
        validate_pixel_dimensions(src_w, src_h)?;
    }

    // Try the pure-Rust `image` crate first.
    match decode_via_image_crate(path, target_w, target_h) {
        Ok(img) => return Ok(img),
        Err(e) => {
            tracing::debug!(
                "image crate failed for {:?}: {} — trying WIC fallback",
                path,
                e
            );
        }
    }

    // WIC fallback (handles HEIC and other Windows-codec formats).
    decode_via_wic(path, target_w, target_h)
        .with_context(|| format!("WIC fallback also failed for {:?}", path))
}

/// Scale dimensions to fit within (max_w, max_h) preserving aspect ratio.
/// Never upscales.
fn fit_dimensions(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (src_w, src_h);
    }
    // Only downscale
    if src_w <= max_w && src_h <= max_h {
        return (src_w, src_h);
    }
    let scale_w = max_w as f64 / src_w as f64;
    let scale_h = max_h as f64 / src_h as f64;
    let scale = scale_w.min(scale_h);
    let new_w = ((src_w as f64 * scale) as u32).max(1);
    let new_h = ((src_h as f64 * scale) as u32).max(1);
    (new_w, new_h)
}

fn decode_via_image_crate(path: &Path, target_w: u32, target_h: u32) -> Result<DecodedImage> {
    use image::imageops::FilterType;

    let img = image::open(path).with_context(|| format!("image::open({:?})", path))?;

    let (src_w, src_h) = (img.width(), img.height());
    let (new_w, new_h) = fit_dimensions(src_w, src_h, target_w, target_h);

    let img = if (new_w, new_h) != (src_w, src_h) {
        img.resize(new_w, new_h, FilterType::Lanczos3)
    } else {
        img
    };

    // Convert to BGRA
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for pixel in rgba.pixels() {
        bgra.push(pixel[2]); // B
        bgra.push(pixel[1]); // G
        bgra.push(pixel[0]); // R
        bgra.push(pixel[3]); // A
    }

    Ok(DecodedImage {
        width: w,
        height: h,
        bgra,
    })
}

fn decode_via_wic(path: &Path, target_w: u32, target_h: u32) -> Result<DecodedImage> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICBitmapDecoder, IWICBitmapScaler,
        IWICFormatConverter, IWICImagingFactory, WICBitmapDitherTypeNone,
        WICBitmapInterpolationModeFant, WICBitmapPaletteTypeMedianCut,
        WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    // Encode path as null-terminated UTF-16
    let path_wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();

    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(WICImagingFactory)")?;

        let decoder: IWICBitmapDecoder = factory
            .CreateDecoderFromFilename(
                PCWSTR::from_raw(path_wide.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .context("WIC CreateDecoderFromFilename")?;

        let frame = decoder.GetFrame(0).context("WIC GetFrame(0)")?;

        let mut src_w = 0u32;
        let mut src_h = 0u32;
        frame
            .GetSize(&mut src_w, &mut src_h)
            .context("WIC GetSize")?;

        validate_pixel_dimensions(src_w, src_h)?;

        let (new_w, new_h) = fit_dimensions(src_w, src_h, target_w, target_h);

        // Scale via IWICBitmapScaler
        let scaler: IWICBitmapScaler = factory
            .CreateBitmapScaler()
            .context("WIC CreateBitmapScaler")?;

        scaler
            .Initialize(&frame, new_w, new_h, WICBitmapInterpolationModeFant)
            .context("WIC scaler Initialize")?;

        // Convert to BGRA32 via IWICFormatConverter
        let converter: IWICFormatConverter = factory
            .CreateFormatConverter()
            .context("WIC CreateFormatConverter")?;

        // pipalette = None is typed as Option<&IWICPalette> — windows crate accepts None here.
        converter
            .Initialize(
                &scaler,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )
            .context("WIC converter Initialize")?;

        let stride = new_w * 4;
        let buf_size = stride * new_h;
        let mut bgra = vec![0u8; buf_size as usize];

        // CopyPixels is on IWICBitmapSource (base of IWICFormatConverter)
        converter
            .CopyPixels(std::ptr::null(), stride, &mut bgra)
            .context("WIC CopyPixels")?;

        Ok(DecodedImage {
            width: new_w,
            height: new_h,
            bgra,
        })
    }
}

// ---------------------------------------------------------------------------
// Simple fixed-capacity LRU cache (VecDeque-based, ~30 lines)
// ---------------------------------------------------------------------------

type CacheKey = (std::path::PathBuf, u32, u32);

pub struct DecodeCache {
    capacity: usize,
    max_bytes: usize,
    bytes: usize,
    entries: VecDeque<(CacheKey, Arc<DecodedImage>)>,
}

impl DecodeCache {
    pub fn new(capacity: usize) -> Self {
        Self::with_byte_budget(capacity, usize::MAX)
    }

    /// Build a count- and byte-bounded cache.  The byte limit is based on the
    /// actual decoded BGRA buffers, rather than a fixed resolution estimate,
    /// so a cache of large-monitor images cannot silently exceed its budget.
    pub fn with_byte_budget(capacity: usize, max_bytes: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            max_bytes,
            bytes: 0,
            entries: VecDeque::new(),
        }
    }

    /// Look up an entry. On hit, move to front (MRU position).
    pub fn get(&mut self, key: &CacheKey) -> Option<Arc<DecodedImage>> {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == key) {
            let entry = self.entries.remove(pos).unwrap();
            let val = Arc::clone(&entry.1);
            self.entries.push_front(entry);
            Some(val)
        } else {
            None
        }
    }

    /// Insert an entry, evicting the LRU tail if at capacity.
    pub fn insert(&mut self, key: CacheKey, value: Arc<DecodedImage>) {
        // Remove existing entry for this key if present
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == &key) {
            if let Some((_, old)) = self.entries.remove(pos) {
                self.bytes = self.bytes.saturating_sub(old.bgra.len());
            }
        }

        let value_bytes = value.bgra.len();
        // A single image larger than the budget is never cacheable.  Keeping
        // it out is preferable to evicting the whole cache and still
        // exceeding the configured memory bound.
        if value_bytes > self.max_bytes {
            return;
        }

        while self.entries.len() >= self.capacity
            || self.bytes.saturating_add(value_bytes) > self.max_bytes
        {
            if let Some((_, old)) = self.entries.pop_back() {
                self.bytes = self.bytes.saturating_sub(old.bgra.len());
            } else {
                break;
            }
        }
        self.bytes = self.bytes.saturating_add(value_bytes);
        self.entries.push_front((key, value));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

// Thread-safe wrapper
pub struct SharedDecodeCache(Mutex<DecodeCache>);

impl SharedDecodeCache {
    pub fn new(capacity: usize) -> Self {
        Self(Mutex::new(DecodeCache::new(capacity)))
    }

    pub fn with_byte_budget(capacity: usize, max_bytes: usize) -> Self {
        Self(Mutex::new(DecodeCache::with_byte_budget(
            capacity, max_bytes,
        )))
    }

    pub fn get_or_decode(
        &self,
        path: &Path,
        target_w: u32,
        target_h: u32,
    ) -> Result<Arc<DecodedImage>> {
        let key = (path.to_path_buf(), target_w, target_h);
        {
            let mut cache = self.0.lock().unwrap();
            if let Some(cached) = cache.get(&key) {
                return Ok(cached);
            }
        }
        let img = Arc::new(decode_image(path, target_w, target_h)?);
        {
            let mut cache = self.0.lock().unwrap();
            cache.insert(key, Arc::clone(&img));
        }
        Ok(img)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal 4×4 red-square JPEG (baseline DCT, no EXIF).
    /// Generated once via image crate and embedded as a byte literal.
    fn tiny_red_jpeg() -> Vec<u8> {
        // Use the image crate at test-time to produce a deterministic byte blob.
        use image::{ImageBuffer, Rgb};
        let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(4, 4, |_, _| Rgb([255u8, 0, 0]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        buf.into_inner()
    }

    #[test]
    fn test_decode_jpeg_returns_bgra() {
        use std::io::Write;
        let jpeg = tiny_red_jpeg();
        let dir = std::env::temp_dir();
        let path = dir.join("aurora_test_red4x4.jpg");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&jpeg).unwrap();
        }
        let img = decode_image(&path, 1920, 1080).expect("decode should succeed");
        assert_eq!(img.width, 4);
        assert_eq!(img.height, 4);
        assert_eq!(img.bgra.len(), 4 * 4 * 4);
        // JPEG is lossy, just check the dominant channel is R (stored as B=low, G=low, R=high)
        // BGRA: B at 0, G at 1, R at 2, A at 3
        let r = img.bgra[2]; // R channel of first pixel
        let b = img.bgra[0]; // B channel
        assert!(
            r > 128,
            "red channel should be dominant, got R={} B={}",
            r,
            b
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_decode_cache_lru() {
        let mut cache = DecodeCache::new(3);
        let dummy = |v: u8| {
            Arc::new(DecodedImage {
                width: 1,
                height: 1,
                bgra: vec![v, v, v, 255],
            })
        };
        let k = |n: u32| {
            (
                std::path::PathBuf::from(format!("img{}.jpg", n)),
                10u32,
                10u32,
            )
        };

        cache.insert(k(1), dummy(1));
        cache.insert(k(2), dummy(2));
        cache.insert(k(3), dummy(3));
        assert_eq!(cache.len(), 3);

        // Inserting a 4th evicts LRU (k(1))
        cache.insert(k(4), dummy(4));
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&k(1)).is_none(), "k(1) should have been evicted");
        assert!(cache.get(&k(4)).is_some());
    }

    #[test]
    fn test_decode_cache_byte_budget_evicts_actual_size() {
        let mut cache = DecodeCache::with_byte_budget(8, 10);
        let image = |size: usize| {
            Arc::new(DecodedImage {
                width: 1,
                height: 1,
                bgra: vec![0; size],
            })
        };
        let key = |n: u32| (std::path::PathBuf::from(format!("img{}.jpg", n)), 1, 1);

        cache.insert(key(1), image(8));
        assert_eq!(cache.bytes(), 8);
        cache.insert(key(2), image(4));

        // The second image does not fit beside the first, so the LRU is
        // evicted and the byte budget remains intact.
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), 4);
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(2)).is_some());
    }

    #[test]
    fn test_decode_cache_skips_single_oversized_image() {
        let mut cache = DecodeCache::with_byte_budget(2, 4);
        let image = Arc::new(DecodedImage {
            width: 1,
            height: 1,
            bgra: vec![0; 5],
        });

        cache.insert((std::path::PathBuf::from("too-large.jpg"), 1, 1), image);
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn test_fit_dimensions_no_upscale() {
        let (w, h) = fit_dimensions(800, 600, 1920, 1080);
        assert_eq!((w, h), (800, 600), "should not upscale");
    }

    #[test]
    fn test_fit_dimensions_downscale() {
        let (w, h) = fit_dimensions(3840, 2160, 1920, 1080);
        assert_eq!((w, h), (1920, 1080));
    }

    #[test]
    fn test_fit_dimensions_aspect_preserved() {
        // 4K wide → fit in 1280×720 box
        let (w, h) = fit_dimensions(3840, 1080, 1280, 720);
        assert_eq!(w, 1280);
        // height should be proportional: 1080 * (1280/3840) = 360
        assert_eq!(h, 360);
    }

    #[test]
    fn test_validate_pixel_dimensions_rejects_pixel_bomb() {
        let err = validate_pixel_dimensions(10_000, 10_001).unwrap_err();
        assert!(err.to_string().contains("exceed maximum"));
    }

    #[test]
    fn test_decode_rejects_oversized_file_before_reading_contents() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(MAX_IMAGE_FILE_BYTES + 1).unwrap();

        let err = decode_image(file.path(), 1920, 1080).unwrap_err();
        assert!(err.to_string().contains("maximum is"));
    }
}

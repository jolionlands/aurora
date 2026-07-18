use crate::metrics::Metrics;
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
    validate_file_metadata(path, &metadata)
}

fn validate_file_metadata(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
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
///
/// Native formats use `image::image_dimensions`. If that fails, WIC opens the
/// first frame and proves that one BGRA pixel can be decoded. Callers that may
/// pass WIC-only formats must initialize COM on the current thread first.
pub fn validate_image_file(path: &Path) -> Result<(u32, u32)> {
    validate_file_size(path)?;
    match image::image_dimensions(path) {
        Ok(dimensions) => {
            validate_pixel_dimensions(dimensions.0, dimensions.1)?;
            Ok(dimensions)
        }
        Err(image_error) => {
            tracing::debug!(
                "image crate could not read dimensions for {:?}: {} — trying WIC fallback",
                path,
                image_error
            );
            let frame = open_wic_frame(path).with_context(|| {
                format!(
                    "read image dimensions for {:?} (image crate failed: {})",
                    path, image_error
                )
            })?;
            validate_pixel_dimensions(frame.width, frame.height)?;
            copy_wic_bgra(&frame, 1, 1).context("decode one WIC pixel")?;
            Ok((frame.width, frame.height))
        }
    }
}

// ---------------------------------------------------------------------------
// Decode pipeline
// ---------------------------------------------------------------------------

/// Decode `path` to a BGRA buffer scaled to fit within `(target_w, target_h)`
/// while preserving aspect ratio.  Never upscales.
///
/// Primary path: `image` crate (JPEG, PNG, GIF, WebP, TIFF, BMP, ICO).
/// Fallback path: Windows Imaging Component (WIC) for explicitly configured
/// formats such as AVIF, HEIC, and HEIF when WIC can decode that exact file.
/// Callers using the fallback must initialize COM on the current thread.
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

    // Best-effort WIC fallback for explicitly configured or direct formats.
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

    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let mut bgra = rgba.into_raw();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    Ok(DecodedImage {
        width: w,
        height: h,
        bgra,
    })
}

struct WicFrame {
    factory: windows::Win32::Graphics::Imaging::IWICImagingFactory,
    _decoder: windows::Win32::Graphics::Imaging::IWICBitmapDecoder,
    frame: windows::Win32::Graphics::Imaging::IWICBitmapFrameDecode,
    width: u32,
    height: u32,
}

fn open_wic_frame(path: &Path) -> Result<WicFrame> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, IWICBitmapDecoder, IWICImagingFactory,
        WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    let path = HSTRING::from(path);

    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(WICImagingFactory)")?;
        let decoder: IWICBitmapDecoder = factory
            .CreateDecoderFromFilename(&path, None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .context("WIC CreateDecoderFromFilename")?;
        let frame = decoder.GetFrame(0).context("WIC GetFrame(0)")?;
        let mut width = 0;
        let mut height = 0;
        frame
            .GetSize(&mut width, &mut height)
            .context("WIC GetSize")?;

        Ok(WicFrame {
            factory,
            _decoder: decoder,
            frame,
            width,
            height,
        })
    }
}

fn copy_wic_bgra(wic: &WicFrame, width: u32, height: u32) -> Result<Vec<u8>> {
    use windows::Win32::Graphics::Imaging::{
        GUID_WICPixelFormat32bppBGRA, IWICBitmapScaler, IWICFormatConverter,
        WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeMedianCut,
    };

    unsafe {
        // Scale via IWICBitmapScaler
        let scaler: IWICBitmapScaler = wic
            .factory
            .CreateBitmapScaler()
            .context("WIC CreateBitmapScaler")?;

        scaler
            .Initialize(&wic.frame, width, height, WICBitmapInterpolationModeFant)
            .context("WIC scaler Initialize")?;

        // Convert to BGRA32 via IWICFormatConverter
        let converter: IWICFormatConverter = wic
            .factory
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

        let stride = width * 4;
        let buf_size = stride * height;
        let mut bgra = vec![0u8; buf_size as usize];

        // CopyPixels is on IWICBitmapSource (base of IWICFormatConverter)
        converter
            .CopyPixels(std::ptr::null(), stride, &mut bgra)
            .context("WIC CopyPixels")?;

        Ok(bgra)
    }
}

fn decode_via_wic(path: &Path, target_w: u32, target_h: u32) -> Result<DecodedImage> {
    let wic = open_wic_frame(path)?;
    validate_pixel_dimensions(wic.width, wic.height)?;
    let (width, height) = fit_dimensions(wic.width, wic.height, target_w, target_h);
    let bgra = copy_wic_bgra(&wic, width, height)?;

    Ok(DecodedImage {
        width,
        height,
        bgra,
    })
}

// ---------------------------------------------------------------------------
// Simple fixed-capacity LRU cache (VecDeque-based, ~30 lines)
// ---------------------------------------------------------------------------

/// Content-backed cache identity.
// ponytail: cache hits hash the full source file; use filesystem change-journal
// invalidation before caching fingerprints if profiling shows this is material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature([u8; 32]);

impl FileSignature {
    fn read(path: &Path) -> Result<Self> {
        use std::io::Read;

        let file = std::fs::File::open(path)
            .with_context(|| format!("open image for cache signature {:?}", path))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("read cache metadata for {:?}", path))?;
        validate_file_metadata(path, &metadata)?;

        let mut reader = file.take(MAX_IMAGE_FILE_BYTES + 1);
        let mut hasher = blake3::Hasher::new();
        hasher
            .update_reader(&mut reader)
            .with_context(|| format!("hash image for cache signature {:?}", path))?;
        if reader.limit() == 0 {
            bail!(
                "image file {:?} grew beyond the maximum of {} bytes while hashing",
                path,
                MAX_IMAGE_FILE_BYTES
            );
        }
        Ok(Self(*hasher.finalize().as_bytes()))
    }

    fn still_matches(self, path: &Path) -> bool {
        Self::read(path).is_ok_and(|current| current == self)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CacheKey {
    path: std::path::PathBuf,
    target_w: u32,
    target_h: u32,
    signature: FileSignature,
}

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
    fn get(&mut self, key: &CacheKey) -> Option<Arc<DecodedImage>> {
        let pos = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == key)?;
        let entry = self.entries.remove(pos)?;
        let value = Arc::clone(&entry.1);
        self.entries.push_front(entry);
        Some(value)
    }

    /// Insert an entry, evicting the LRU tail if at capacity.
    fn insert(&mut self, key: CacheKey, value: Arc<DecodedImage>) {
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
pub struct SharedDecodeCache {
    cache: Mutex<DecodeCache>,
    metrics: Arc<Metrics>,
}

impl SharedDecodeCache {
    pub fn new(capacity: usize, metrics: Arc<Metrics>) -> Self {
        Self {
            cache: Mutex::new(DecodeCache::new(capacity)),
            metrics,
        }
    }

    pub fn with_byte_budget(capacity: usize, max_bytes: usize, metrics: Arc<Metrics>) -> Self {
        Self {
            cache: Mutex::new(DecodeCache::with_byte_budget(capacity, max_bytes)),
            metrics,
        }
    }

    fn insert_if_unchanged(&self, path: &Path, key: CacheKey, image: Arc<DecodedImage>) {
        if key.signature.still_matches(path) {
            self.cache.lock().unwrap().insert(key, image);
        }
    }

    pub fn get_or_decode(
        &self,
        path: &Path,
        target_w: u32,
        target_h: u32,
    ) -> Result<Arc<DecodedImage>> {
        validate_file_size(path)?;
        let key = CacheKey {
            path: path.to_path_buf(),
            target_w,
            target_h,
            signature: FileSignature::read(path)?,
        };
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(cached) = cache.get(&key) {
                self.metrics.record_cache_hit();
                return Ok(cached);
            }
        }
        self.metrics.record_cache_miss();
        let img = Arc::new(decode_image(path, target_w, target_h)?);
        self.insert_if_unchanged(path, key, Arc::clone(&img));
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

    fn cache_key(n: u32) -> CacheKey {
        CacheKey {
            path: std::path::PathBuf::from(format!("img{n}.jpg")),
            target_w: 10,
            target_h: 10,
            signature: FileSignature([n as u8; 32]),
        }
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
    fn wic_validation_decodes_one_bgra_pixel() {
        use image::{ImageBuffer, ImageFormat, Rgb};
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

        struct ComApartment;
        impl Drop for ComApartment {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }

        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        assert!(
            initialized.is_ok(),
            "CoInitializeEx failed: {initialized:?}"
        );
        let _com = ComApartment;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wallpaper.heic");
        let image: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(2, 2, Rgb([255, 0, 0]));
        image.save_with_format(&path, ImageFormat::Bmp).unwrap();

        assert!(image::image_dimensions(&path).is_err());
        let frame = open_wic_frame(&path).unwrap();
        assert_eq!(copy_wic_bgra(&frame, 1, 1).unwrap().len(), 4);
        assert_eq!(validate_image_file(&path).unwrap(), (2, 2));
    }

    #[test]
    fn shared_cache_records_miss_then_hit() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("red.jpg");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&tiny_red_jpeg())
            .unwrap();

        let metrics = Metrics::new();
        let cache = SharedDecodeCache::new(2, Arc::clone(&metrics));
        cache.get_or_decode(&path, 4, 4).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        cache.get_or_decode(&path, 4, 4).unwrap();

        assert_eq!(
            metrics
                .cache_misses
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            metrics
                .cache_hits
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(metrics.cache_hit_ratio(), 0.5);
    }

    #[test]
    fn shared_cache_invalidates_same_length_and_mtime_rewrite() {
        use image::{ImageBuffer, Rgb};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wallpaper.bmp");
        let red: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(2, 2, Rgb([255, 0, 0]));
        red.save(&path).unwrap();

        let metrics = Metrics::new();
        let cache = SharedDecodeCache::new(2, Arc::clone(&metrics));
        let first = cache.get_or_decode(&path, 10, 10).unwrap();
        let first_metadata = std::fs::metadata(&path).unwrap();

        let blue: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(2, 2, Rgb([0, 0, 255]));
        blue.save(&path).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(first_metadata.modified().unwrap()))
            .unwrap();
        let second_metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(first_metadata.len(), second_metadata.len());
        assert_eq!(
            first_metadata.modified().unwrap(),
            second_metadata.modified().unwrap()
        );
        let second = cache.get_or_decode(&path, 10, 10).unwrap();

        assert_eq!((first.width, first.height), (2, 2));
        assert_eq!((second.width, second.height), (2, 2));
        assert!(
            second.bgra[0] > second.bgra[2],
            "rewritten pixels must be blue"
        );
        assert_eq!(
            metrics
                .cache_misses
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }

    #[test]
    fn changed_signature_is_not_cached_after_decode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wallpaper.jpg");
        std::fs::write(&path, b"before").unwrap();
        let before = FileSignature::read(&path).unwrap();
        assert!(before.still_matches(&path));
        let key = CacheKey {
            path: path.clone(),
            target_w: 1,
            target_h: 1,
            signature: before,
        };

        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_IMAGE_FILE_BYTES + 1)
            .unwrap();
        assert!(FileSignature::read(&path)
            .unwrap_err()
            .to_string()
            .contains("maximum"));

        let cache = SharedDecodeCache::new(1, Metrics::new());
        cache.insert_if_unchanged(
            &path,
            key,
            Arc::new(DecodedImage {
                width: 1,
                height: 1,
                bgra: vec![0, 0, 0, 255],
            }),
        );

        assert!(!before.still_matches(&path));
        assert!(cache.cache.lock().unwrap().is_empty());
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
        cache.insert(cache_key(1), dummy(1));
        cache.insert(cache_key(2), dummy(2));
        cache.insert(cache_key(3), dummy(3));
        assert_eq!(cache.len(), 3);

        // Inserting a 4th evicts LRU (k(1))
        cache.insert(cache_key(4), dummy(4));
        assert_eq!(cache.len(), 3);
        assert!(
            cache.get(&cache_key(1)).is_none(),
            "k(1) should have been evicted"
        );
        assert!(cache.get(&cache_key(4)).is_some());
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
        cache.insert(cache_key(1), image(8));
        assert_eq!(cache.bytes(), 8);
        cache.insert(cache_key(2), image(4));

        // The second image does not fit beside the first, so the LRU is
        // evicted and the byte budget remains intact.
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), 4);
        assert!(cache.get(&cache_key(1)).is_none());
        assert!(cache.get(&cache_key(2)).is_some());
    }

    #[test]
    fn test_decode_cache_skips_single_oversized_image() {
        let mut cache = DecodeCache::with_byte_budget(2, 4);
        let image = Arc::new(DecodedImage {
            width: 1,
            height: 1,
            bgra: vec![0; 5],
        });

        cache.insert(cache_key(1), image);
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

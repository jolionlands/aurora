use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;

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
    // Try the pure-Rust `image` crate first.
    match decode_via_image_crate(path, target_w, target_h) {
        Ok(img) => return Ok(img),
        Err(e) => {
            tracing::debug!("image crate failed for {:?}: {} — trying WIC fallback", path, e);
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
    use image::DynamicImage;

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

    Ok(DecodedImage { width: w, height: h, bgra })
}

fn decode_via_wic(path: &Path, target_w: u32, target_h: u32) -> Result<DecodedImage> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, IWICBitmapDecoder, IWICBitmapScaler, IWICFormatConverter,
        IWICImagingFactory, GUID_WICPixelFormat32bppBGRA,
        WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
        WICBitmapPaletteTypeMedianCut, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    // Encode path as null-terminated UTF-16
    let path_wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();

    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(
            &CLSID_WICImagingFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .context("CoCreateInstance(WICImagingFactory)")?;

        let decoder: IWICBitmapDecoder = factory
            .CreateDecoderFromFilename(
                PCWSTR::from_raw(path_wide.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .context("WIC CreateDecoderFromFilename")?;

        let frame = decoder
            .GetFrame(0)
            .context("WIC GetFrame(0)")?;

        let mut src_w = 0u32;
        let mut src_h = 0u32;
        frame.GetSize(&mut src_w, &mut src_h).context("WIC GetSize")?;

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

        Ok(DecodedImage { width: new_w, height: new_h, bgra })
    }
}

// ---------------------------------------------------------------------------
// Simple fixed-capacity LRU cache (VecDeque-based, ~30 lines)
// ---------------------------------------------------------------------------

type CacheKey = (std::path::PathBuf, u32, u32);

pub struct DecodeCache {
    capacity: usize,
    entries: VecDeque<(CacheKey, Arc<DecodedImage>)>,
}

impl DecodeCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
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
            self.entries.remove(pos);
        }
        if self.entries.len() >= self.capacity {
            self.entries.pop_back();
        }
        self.entries.push_front((key, value));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// Thread-safe wrapper
pub struct SharedDecodeCache(Mutex<DecodeCache>);

impl SharedDecodeCache {
    pub fn new(capacity: usize) -> Self {
        Self(Mutex::new(DecodeCache::new(capacity)))
    }

    pub fn get_or_decode(&self, path: &Path, target_w: u32, target_h: u32) -> Result<Arc<DecodedImage>> {
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
        assert!(r > 128, "red channel should be dominant, got R={} B={}", r, b);
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
        let k = |n: u32| (std::path::PathBuf::from(format!("img{}.jpg", n)), 10u32, 10u32);

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
}

use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::types::SourceConfig;
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use rand::Rng;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PhotoEntry {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub mtime: SystemTime,
    pub exif_date: Option<SystemTime>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Star rating 0–5, read from XMP sidecar (<basename>.xmp)
    pub rating: Option<u8>,
    /// Blake3 hash of first 8 KB — hex string
    pub hash: String,
    pub banned: bool,
}

#[derive(Debug, Default)]
pub struct PhotoIndex {
    pub photos: Vec<PhotoEntry>,
    /// hash → index into `photos`
    pub by_hash: HashMap<String, usize>,
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

impl PhotoIndex {
    /// Scan one or more root directories and build the index.
    pub fn scan(roots: &[PathBuf], extensions: &[String], recursive: bool) -> Result<Self> {
        let mut index = PhotoIndex::default();
        let exts: Vec<String> = extensions.iter().map(|e| e.to_lowercase()).collect();

        for root in roots {
            collect_files(root, &exts, recursive, 0, 0, &mut index)?;
        }

        Ok(index)
    }

    /// Scan configured sources, preserving each source's extension, recursion,
    /// and minimum-dimension rules.
    pub fn scan_sources(sources: &[SourceConfig]) -> Result<Self> {
        let mut index = PhotoIndex::default();

        for source in sources {
            let exts: Vec<String> = source.extensions.iter().map(|e| e.to_lowercase()).collect();
            collect_files(
                &source.path,
                &exts,
                source.recursive,
                source.min_width,
                source.min_height,
                &mut index,
            )?;
        }

        Ok(index)
    }

    /// Attach a file-system watcher.  The caller handles events via the channel
    /// returned by `notify`.  Pass a channel sender compatible with `notify`'s
    /// `EventHandler` trait (e.g. `std::sync::mpsc::Sender<notify::Result<notify::Event>>`).
    pub fn watch(&mut self) -> Result<RecommendedWatcher> {
        // We return a watcher that the caller must keep alive.
        // The caller is responsible for re-scanning when events arrive.
        let (tx, _rx) = std::sync::mpsc::channel();
        let watcher = RecommendedWatcher::new(tx, NotifyConfig::default())
            .context("failed to create filesystem watcher")?;
        Ok(watcher)
    }

    /// Watch a specific list of root paths.
    pub fn watch_roots(
        &mut self,
        roots: &[PathBuf],
    ) -> Result<(
        RecommendedWatcher,
        std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    )> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = RecommendedWatcher::new(tx, NotifyConfig::default())
            .context("failed to create filesystem watcher")?;
        for root in roots {
            let mode = RecursiveMode::Recursive;
            watcher
                .watch(root, mode)
                .with_context(|| format!("failed to watch {:?}", root))?;
        }
        Ok((watcher, rx))
    }

    // -----------------------------------------------------------------------
    // Pickers
    // -----------------------------------------------------------------------

    /// Pick a random, non-banned photo that is not in the last `recent_window`
    /// entries of the provided recency tracker.
    pub fn pick_random(
        &self,
        recent_window: usize,
        recent_paths: &VecDeque<PathBuf>,
    ) -> Option<&PhotoEntry> {
        // Cap the anti-repeat window so we never exclude every photo.
        let effective_window = recent_window.min(self.photos.len().saturating_sub(1));
        let recent_path_set: std::collections::HashSet<&PathBuf> =
            recent_paths.iter().rev().take(effective_window).collect();
        let recent_hashes = self.recent_hashes(&recent_path_set);

        let candidates: Vec<usize> = self
            .photos
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                !e.banned && !recent_path_set.contains(&e.path) && !recent_hashes.contains(&e.hash)
            })
            .map(|(i, _)| i)
            .collect();

        if candidates.is_empty() {
            // Small library: every photo is in the recent window -- fall back to all non-banned.
            let all: Vec<usize> = self
                .photos
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.banned)
                .map(|(i, _)| i)
                .collect();
            if all.is_empty() {
                return None;
            }
            let idx = all[rand::thread_rng().gen_range(0..all.len())];
            return Some(&self.photos[idx]);
        }

        let idx = candidates[rand::thread_rng().gen_range(0..candidates.len())];
        Some(&self.photos[idx])
    }

    /// Pick the next photo after `current_idx` (wrapping), skipping banned ones.
    pub fn pick_next_sequential(&self, current_idx: usize) -> Option<&PhotoEntry> {
        let len = self.photos.len();
        if len == 0 {
            return None;
        }
        let start = (current_idx + 1) % len;
        for offset in 0..len {
            let i = (start + offset) % len;
            if !self.photos[i].banned {
                return Some(&self.photos[i]);
            }
        }
        None
    }

    /// Pick a random photo weighted by star rating.  Unrated photos get weight 1,
    /// rated photos get weight `rating + 1` (so 5 stars → 6×).
    pub fn pick_weighted_by_rating(
        &self,
        recent_window: usize,
        recent_paths: &VecDeque<PathBuf>,
    ) -> Option<&PhotoEntry> {
        let effective_window = recent_window.min(self.photos.len().saturating_sub(1));
        let recent_path_set: std::collections::HashSet<&PathBuf> =
            recent_paths.iter().rev().take(effective_window).collect();
        let recent_hashes = self.recent_hashes(&recent_path_set);

        let candidates: Vec<(usize, u32)> = self
            .photos
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                !e.banned && !recent_path_set.contains(&e.path) && !recent_hashes.contains(&e.hash)
            })
            .map(|(i, e)| {
                let w = e.rating.map(|r| r as u32 + 1).unwrap_or(1);
                (i, w)
            })
            .collect();

        if candidates.is_empty() {
            // Fall back to all non-banned photos.
            let all: Vec<(usize, u32)> = self
                .photos
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.banned)
                .map(|(i, e)| {
                    let w = e.rating.map(|r| r as u32 + 1).unwrap_or(1);
                    (i, w)
                })
                .collect();
            if all.is_empty() {
                return None;
            }
            let total: u64 = all
                .iter()
                .map(|(_, w)| u64::from(*w))
                .fold(0, u64::saturating_add);
            let mut pick = rand::thread_rng().gen_range(0..total);
            for (idx, w) in &all {
                let weight = u64::from(*w);
                if pick < weight {
                    return Some(&self.photos[*idx]);
                }
                pick -= weight;
            }
            return Some(&self.photos[all.last().unwrap().0]);
        }

        let total: u64 = candidates
            .iter()
            .map(|(_, w)| u64::from(*w))
            .fold(0, u64::saturating_add);
        let mut pick = rand::thread_rng().gen_range(0..total);
        for (idx, w) in &candidates {
            let weight = u64::from(*w);
            if pick < weight {
                return Some(&self.photos[*idx]);
            }
            pick -= weight;
        }
        Some(&self.photos[candidates.last().unwrap().0])
    }

    // -----------------------------------------------------------------------
    // Mutations
    // -----------------------------------------------------------------------

    pub fn ban(&mut self, hash: &str) -> bool {
        let mut found = false;
        for entry in &mut self.photos {
            if entry.hash == hash {
                entry.banned = true;
                found = true;
            }
        }
        found
    }

    pub fn rate(&mut self, idx: usize, stars: u8) {
        if let Some(entry) = self.photos.get_mut(idx) {
            entry.rating = Some(stars.min(5));
        }
    }

    pub fn len(&self) -> usize {
        self.photos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.photos.is_empty()
    }

    fn recent_hashes(
        &self,
        recent_paths: &std::collections::HashSet<&PathBuf>,
    ) -> std::collections::HashSet<String> {
        self.photos
            .iter()
            .filter(|entry| recent_paths.contains(&entry.path))
            .map(|entry| entry.hash.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn collect_files(
    dir: &Path,
    extensions: &[String],
    recursive: bool,
    min_width: u32,
    min_height: u32,
    index: &mut PhotoIndex,
) -> Result<()> {
    let read_dir =
        std::fs::read_dir(dir).with_context(|| format!("cannot read directory {:?}", dir))?;

    for entry in read_dir {
        let entry = entry.context("error reading directory entry")?;
        let path = entry.path();
        let meta = entry.metadata().context("metadata error")?;

        if meta.is_dir() {
            if recursive {
                collect_files(&path, extensions, true, min_width, min_height, index)?;
            }
            continue;
        }

        if !meta.is_file() {
            continue;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        if !extensions.contains(&ext) {
            continue;
        }

        match build_entry(&path, &meta, min_width, min_height) {
            Ok(Some(photo)) => {
                let hash = photo.hash.clone();
                let idx = index.photos.len();
                index.photos.push(photo);
                index.by_hash.entry(hash).or_insert(idx);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("skipping image {}: {}", path.display(), e);
            }
        }
    }

    Ok(())
}

fn build_entry(
    path: &Path,
    meta: &std::fs::Metadata,
    min_width: u32,
    min_height: u32,
) -> Result<Option<PhotoEntry>> {
    let size_bytes = meta.len();
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let (verified_width, verified_height) = validate_image_dimensions(path)
        .with_context(|| format!("invalid or unsupported image {}", path.display()))?;
    if verified_width < min_width || verified_height < min_height {
        return Ok(None);
    }

    let hash = hash_file(path)?;
    let (exif_date, exif_width, exif_height) = read_exif_dims(path);
    let width = exif_width.or(Some(verified_width));
    let height = exif_height.or(Some(verified_height));
    let rating = read_xmp_rating(path);

    Ok(Some(PhotoEntry {
        path: path.to_path_buf(),
        size_bytes,
        mtime,
        exif_date,
        width,
        height,
        rating,
        hash,
        banned: false,
    }))
}

/// Blake3 hash of first 8 KB — returned as lowercase hex.
fn hash_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot open {:?} for hashing", path))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).context("read error")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn validate_image_dimensions(path: &Path) -> Result<(u32, u32)> {
    match image::image_dimensions(path) {
        Ok(dims) => Ok(dims),
        Err(e) => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if ext == "heic" || ext == "heif" {
                Ok((u32::MAX, u32::MAX))
            } else {
                Err(e.into())
            }
        }
    }
}

/// Read EXIF DateTimeOriginal and image dimensions.
/// Returns (exif_date, width, height); any or all can be None on failure.
fn read_exif_dims(path: &Path) -> (Option<SystemTime>, Option<u32>, Option<u32>) {
    use std::io::BufReader;

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None, None),
    };
    let mut bufreader = BufReader::new(file);
    let exif_reader = exif::Reader::new();
    let exif_data = match exif_reader.read_from_container(&mut bufreader) {
        Ok(e) => e,
        Err(_) => return (None, None, None),
    };

    // DateTimeOriginal
    let exif_date = exif_data
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .and_then(|f| {
            if let exif::Value::Ascii(ref v) = f.value {
                v.first().and_then(|s| parse_exif_datetime(s))
            } else {
                None
            }
        });

    // PixelXDimension / PixelYDimension (EXIF active area dims)
    let width = exif_data
        .get_field(exif::Tag::PixelXDimension, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0));
    let height = exif_data
        .get_field(exif::Tag::PixelYDimension, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0));

    (exif_date, width, height)
}

/// Parse "YYYY:MM:DD HH:MM:SS" → SystemTime.
fn parse_exif_datetime(s: &[u8]) -> Option<SystemTime> {
    let s = std::str::from_utf8(s).ok()?;
    // format: "2023:07:04 14:30:00"
    if s.len() < 19 {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: u32 = s[11..13].parse().ok()?;
    let min: u32 = s[14..16].parse().ok()?;
    let sec: u32 = s[17..19].parse().ok()?;

    use chrono::{TimeZone, Utc};
    let dt = Utc
        .with_ymd_and_hms(year, month, day, hour, min, sec)
        .single()?;
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp() as u64))
}

/// Read XMP sidecar (<basename>.xmp) and extract xmp:Rating.
fn read_xmp_rating(path: &Path) -> Option<u8> {
    let xmp_path = path.with_extension("xmp");
    let content = std::fs::read_to_string(&xmp_path).ok()?;
    // Simple substring search — avoid pulling in an XML parser.
    let tag = "xmp:Rating>";
    let start = content.find(tag)? + tag.len();
    let end = content[start..].find('<')? + start;
    let rating: u8 = content[start..end].trim().parse().ok()?;
    Some(rating.min(5))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

    fn make_temp_jpg(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(16, 16, |_, _| Rgb([255u8, 0, 0]));
        img.save(&p).unwrap();
        p
    }

    #[test]
    fn test_index_scan_finds_jpegs() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_temp_jpg(dir.path(), "a.jpg");
        make_temp_jpg(dir.path(), "b.jpg");
        // A non-jpeg file — should be excluded
        std::fs::write(dir.path().join("ignore.txt"), b"hello").unwrap();

        let roots = vec![dir.path().to_path_buf()];
        let exts = vec!["jpg".to_string(), "jpeg".to_string()];
        let index = PhotoIndex::scan(&roots, &exts, false).unwrap();
        assert_eq!(index.photos.len(), 2);
    }

    #[test]
    fn test_index_ban_excludes() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_temp_jpg(dir.path(), "x.jpg");

        let roots = vec![dir.path().to_path_buf()];
        let exts = vec!["jpg".to_string()];
        let mut index = PhotoIndex::scan(&roots, &exts, false).unwrap();

        assert_eq!(index.photos.len(), 1);
        let hash = index.photos[0].hash.clone();
        index.ban(&hash);

        let recent: VecDeque<PathBuf> = VecDeque::new();
        let result = index.pick_random(0, &recent);
        assert!(result.is_none(), "banned photo should not be picked");
    }

    #[test]
    fn test_pick_next_sequential_wraps() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_temp_jpg(dir.path(), "1.jpg");
        make_temp_jpg(dir.path(), "2.jpg");
        make_temp_jpg(dir.path(), "3.jpg");

        let roots = vec![dir.path().to_path_buf()];
        let exts = vec!["jpg".to_string()];
        let index = PhotoIndex::scan(&roots, &exts, false).unwrap();

        // Should always find a next entry in a 3-photo index
        let next = index.pick_next_sequential(0);
        assert!(next.is_some());
    }
}

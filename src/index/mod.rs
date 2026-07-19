use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::BufReader;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
    FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
};

use crate::config::types::SourceConfig;
use rand::Rng;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PhotoEntry {
    pub path: PathBuf,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Blake3 hash of the entire file — hex string
    pub hash: String,
    pub banned: bool,
}

#[derive(Debug, Default)]
pub struct PhotoIndex {
    pub photos: Vec<PhotoEntry>,
}

const INDEX_CACHE_VERSION: u32 = 2;
// Bump whenever validate_image_file's acceptance rules change.
const INDEX_VALIDATOR_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileFingerprint {
    size: u64,
    creation_time: i64,
    last_write_time: i64,
    change_time: i64,
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedPhoto {
    path: String,
    #[serde(default)]
    fingerprint: Option<FileFingerprint>,
    width: u32,
    height: u32,
    hash: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexCacheFile {
    version: u32,
    #[serde(default)]
    validator_version: u32,
    entries: Vec<CachedPhoto>,
}

#[derive(Default)]
struct CacheLookup {
    entries: Vec<CachedPhoto>,
    by_path: HashMap<PathBuf, usize>,
}

#[derive(Default)]
struct ScanState {
    index: PhotoIndex,
    admitted_files: HashSet<PathBuf>,
    cache: CacheLookup,
    refreshed: HashMap<PathBuf, CachedPhoto>,
    cache_hits: usize,
    cache_misses: usize,
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

impl PhotoIndex {
    /// Scan one or more root directories and build the index.
    /// WIC-only formats require COM to be initialized on the current thread.
    pub fn scan(roots: &[PathBuf], extensions: &[String], recursive: bool) -> Result<Self> {
        let exts: Vec<String> = extensions.iter().map(|e| e.to_lowercase()).collect();
        let mut visited_dirs = HashSet::new();
        let mut state = ScanState::default();

        for root in roots {
            collect_files(
                root,
                &exts,
                recursive,
                (0, 0),
                &mut visited_dirs,
                &mut state,
            );
        }

        Ok(state.index)
    }

    /// Scan configured sources, preserving each source's extension, recursion,
    /// and minimum-dimension rules. WIC-only formats require COM to be
    /// initialized on the current thread.
    pub fn scan_sources(sources: &[SourceConfig]) -> Result<Self> {
        Ok(scan_sources_with_cache(sources, CacheLookup::default())?.0)
    }

    /// Scan configured sources while reusing previously validated image facts.
    /// Cache I/O is best-effort: bad state falls back to a full scan and a
    /// persistence failure never prevents wallpapers from loading.
    pub fn scan_sources_cached(sources: &[SourceConfig], cache_path: &Path) -> Result<Self> {
        let cache = match load_index_cache(cache_path) {
            Ok(cache) => cache,
            Err(error) => {
                tracing::warn!(
                    "ignoring photo index cache {}: {error:#}",
                    cache_path.display()
                );
                CacheLookup::default()
            }
        };
        let (index, refreshed, hits, misses) = scan_sources_with_cache(sources, cache)?;
        if let Err(error) = persist_index_cache(cache_path, refreshed) {
            tracing::warn!(
                "could not persist photo index cache {}: {error:#}",
                cache_path.display()
            );
        }
        tracing::info!("photo index cache: {hits} reused, {misses} validated");
        Ok(index)
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

    pub fn apply_bans(&mut self, hashes: &HashSet<String>) -> usize {
        let mut applied = 0;
        for entry in &mut self.photos {
            entry.banned = hashes.contains(&entry.hash);
            applied += usize::from(entry.banned);
        }
        applied
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

fn scan_sources_with_cache(
    sources: &[SourceConfig],
    cache: CacheLookup,
) -> Result<(PhotoIndex, Vec<CachedPhoto>, usize, usize)> {
    let mut state = ScanState {
        cache,
        ..ScanState::default()
    };
    let mut accessible_roots = 0usize;

    for source in sources {
        let mut visited_dirs = HashSet::new();
        let exts: Vec<String> = source.extensions.iter().map(|e| e.to_lowercase()).collect();
        if collect_files(
            &source.path,
            &exts,
            source.recursive,
            (source.min_width, source.min_height),
            &mut visited_dirs,
            &mut state,
        ) {
            accessible_roots += 1;
        }
    }

    if !sources.is_empty() && accessible_roots == 0 {
        bail!(
            "none of the {} configured photo source roots could be read",
            sources.len()
        );
    }

    let mut refreshed: Vec<_> = state.refreshed.into_values().collect();
    refreshed.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((state.index, refreshed, state.cache_hits, state.cache_misses))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn collect_files(
    dir: &Path,
    extensions: &[String],
    recursive: bool,
    min_dimensions: (u32, u32),
    visited_dirs: &mut HashSet<PathBuf>,
    state: &mut ScanState,
) -> bool {
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!("skipping unreadable directory {}: {}", dir.display(), error);
            return false;
        }
    };
    if !visited_dirs.insert(canonical_dir) {
        return true;
    }

    let read_dir = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!("skipping unreadable directory {}: {}", dir.display(), error);
            return false;
        }
    };

    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    "skipping unreadable directory entry in {}: {}",
                    dir.display(),
                    error
                );
                continue;
            }
        };
        let path = entry.path();
        let meta = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!("skipping unreadable path {}: {}", path.display(), error);
                continue;
            }
        };

        if meta.is_dir() {
            if recursive {
                collect_files(&path, extensions, true, min_dimensions, visited_dirs, state);
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

        let canonical_file = match std::fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!("skipping unreadable file {}: {}", path.display(), error);
                continue;
            }
        };
        if state.admitted_files.contains(&canonical_file) {
            continue;
        }

        match build_entry_cached(
            &path,
            &canonical_file,
            &meta,
            min_dimensions.0,
            min_dimensions.1,
            state,
        ) {
            Ok(Some(photo)) => {
                state.index.photos.push(photo);
                state.admitted_files.insert(canonical_file);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("skipping image {}: {}", path.display(), e);
            }
        }
    }

    true
}

fn build_entry_cached(
    path: &Path,
    canonical_path: &Path,
    meta: &std::fs::Metadata,
    min_width: u32,
    min_height: u32,
    state: &mut ScanState,
) -> Result<Option<PhotoEntry>> {
    if meta.len() > crate::decode::MAX_IMAGE_FILE_BYTES {
        tracing::warn!(
            "skipping oversized image {}: {} bytes exceeds {} byte limit",
            path.display(),
            meta.len(),
            crate::decode::MAX_IMAGE_FILE_BYTES
        );
        return Ok(None);
    }

    let fingerprint = match FileFingerprint::read(path) {
        Ok(fingerprint) => Some(fingerprint),
        Err(error) => {
            tracing::debug!(
                "photo index cache disabled for {}: {error:#}",
                path.display()
            );
            None
        }
    };
    let cached = state
        .cache
        .find(canonical_path, fingerprint.as_ref())
        .cloned();
    let mut fact = if let Some(mut cached) = cached {
        state.cache_hits += 1;
        cached.path = canonical_path.to_string_lossy().into_owned();
        cached
    } else {
        state.cache_misses += 1;
        let (width, height) = crate::decode::validate_image_file(path)
            .with_context(|| format!("invalid or unsupported image {}", path.display()))?;
        CachedPhoto::new(canonical_path, fingerprint, width, height)
    };

    if fact.width < min_width || fact.height < min_height {
        if fact.fingerprint.is_some() {
            state.refreshed.insert(canonical_path.to_path_buf(), fact);
        }
        return Ok(None);
    }

    let hash = match &fact.hash {
        Some(hash) => hash.clone(),
        None => {
            let hash = hash_file(path)?;
            fact.hash = Some(hash.clone());
            hash
        }
    };
    let (width, height) = (fact.width, fact.height);
    if fact.fingerprint.is_some() {
        state.refreshed.insert(canonical_path.to_path_buf(), fact);
    }
    Ok(Some(PhotoEntry {
        path: path.to_path_buf(),
        width: Some(width),
        height: Some(height),
        hash,
        banned: false,
    }))
}

impl FileFingerprint {
    fn read(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .open(path)
            .with_context(|| format!("open file attributes {}", path.display()))?;
        let handle = HANDLE(file.as_raw_handle());
        let mut basic = FILE_BASIC_INFO::default();
        let mut id = FILE_ID_INFO::default();

        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                std::ptr::from_mut(&mut basic).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        }
        .with_context(|| format!("read change time {}", path.display()))?;
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                std::ptr::from_mut(&mut id).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        }
        .with_context(|| format!("read file identity {}", path.display()))?;

        Ok(Self {
            size: file
                .metadata()
                .with_context(|| format!("read file size {}", path.display()))?
                .len(),
            creation_time: basic.CreationTime,
            last_write_time: basic.LastWriteTime,
            change_time: basic.ChangeTime,
            volume_serial_number: id.VolumeSerialNumber,
            file_id: id.FileId.Identifier,
        })
    }
}

impl CachedPhoto {
    fn new(path: &Path, fingerprint: Option<FileFingerprint>, width: u32, height: u32) -> Self {
        Self {
            path: path.to_string_lossy().into_owned(),
            fingerprint,
            width,
            height,
            hash: None,
        }
    }

    fn is_valid(&self) -> bool {
        self.fingerprint
            .is_some_and(|fingerprint| fingerprint.size <= crate::decode::MAX_IMAGE_FILE_BYTES)
            && self.width > 0
            && self.height > 0
            && u64::from(self.width) * u64::from(self.height) <= crate::decode::MAX_IMAGE_PIXELS
            && self.hash.as_deref().is_none_or(valid_blake3_hash)
    }

    fn matches(&self, fingerprint: &FileFingerprint) -> bool {
        self.fingerprint.as_ref() == Some(fingerprint)
    }
}

impl CacheLookup {
    fn from_entries(entries: Vec<CachedPhoto>) -> Self {
        let mut lookup = Self {
            entries,
            ..Self::default()
        };
        for (index, entry) in lookup.entries.iter().enumerate() {
            lookup.by_path.insert(PathBuf::from(&entry.path), index);
        }
        lookup
    }

    fn find(&self, path: &Path, fingerprint: Option<&FileFingerprint>) -> Option<&CachedPhoto> {
        let fingerprint = fingerprint?;
        let by_path = self
            .by_path
            .get(path)
            .and_then(|index| self.entries.get(*index));
        if by_path.is_some_and(|entry| entry.matches(fingerprint)) {
            return by_path;
        }
        None
    }
}

fn valid_blake3_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn load_index_cache(path: &Path) -> Result<CacheLookup> {
    if !path.exists() {
        return Ok(CacheLookup::default());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("read photo index cache {}", path.display()))?;
    let cache: IndexCacheFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse photo index cache {}", path.display()))?;
    if cache.version != INDEX_CACHE_VERSION || cache.validator_version != INDEX_VALIDATOR_VERSION {
        tracing::info!(
            "photo index cache {} uses format/validator {}/{}, rebuilding {}/{}",
            path.display(),
            cache.version,
            cache.validator_version,
            INDEX_CACHE_VERSION,
            INDEX_VALIDATOR_VERSION
        );
        return Ok(CacheLookup::default());
    }
    Ok(CacheLookup::from_entries(
        cache
            .entries
            .into_iter()
            .filter(CachedPhoto::is_valid)
            .collect(),
    ))
}

fn persist_index_cache(path: &Path, entries: Vec<CachedPhoto>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create photo index cache directory {}", parent.display()))?;
    }
    let cache = IndexCacheFile {
        version: INDEX_CACHE_VERSION,
        validator_version: INDEX_VALIDATOR_VERSION,
        entries,
    };
    let bytes = serde_json::to_vec(&cache).context("serialize photo index cache")?;
    let tmp = path.with_extension("json.tmp");
    crate::playlist::write_synced(&tmp, &bytes)?;
    crate::playlist::replace_file(&tmp, path)
}

/// Blake3 hash of the entire file, returned as lowercase hex.
pub(crate) fn hash_file(path: &Path) -> Result<String> {
    // ponytail: fixed read-ahead; retune only from cold-library scan measurements.
    const READ_BUFFER_BYTES: usize = 1024 * 1024;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN.0)
        .open(path)
        .with_context(|| format!("cannot open {:?} for hashing", path))?;
    let mut file = BufReader::with_capacity(READ_BUFFER_BYTES, file);
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&mut file).context("read error")?;
    Ok(hasher.finalize().to_hex().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

    fn make_temp_image(dir: &std::path::Path, name: &str, width: u32, height: u32) -> PathBuf {
        let path = dir.join(name);
        let image: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(width, height, |_, _| Rgb([255u8, 0, 0]));
        image.save(&path).unwrap();
        path
    }

    fn make_temp_jpg(dir: &std::path::Path, name: &str) -> PathBuf {
        make_temp_image(dir, name, 16, 16)
    }

    #[test]
    fn hash_file_matches_blake3() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"aurora").unwrap();
        assert_eq!(
            hash_file(file.path()).unwrap(),
            blake3::hash(b"aurora").to_hex().to_string()
        );
    }

    fn scan_with_test_cache(
        sources: &[SourceConfig],
        cache_path: &Path,
    ) -> (PhotoIndex, usize, usize) {
        let cache = load_index_cache(cache_path).unwrap();
        let (index, refreshed, hits, misses) = scan_sources_with_cache(sources, cache).unwrap();
        persist_index_cache(cache_path, refreshed).unwrap();
        (index, hits, misses)
    }

    fn source(
        path: PathBuf,
        recursive: bool,
        extensions: &[&str],
        min_width: u32,
        min_height: u32,
    ) -> SourceConfig {
        SourceConfig {
            path,
            recursive,
            extensions: extensions.iter().map(|ext| (*ext).to_string()).collect(),
            min_width,
            min_height,
        }
    }

    fn write_bmp_header(path: &Path, width: u32, height: u32) {
        let row_bytes = (width * 3).div_ceil(4) * 4;
        let image_bytes = row_bytes * height;
        let mut header = [0u8; 54];
        header[0..2].copy_from_slice(b"BM");
        header[2..6].copy_from_slice(&(54 + image_bytes).to_le_bytes());
        header[10..14].copy_from_slice(&54u32.to_le_bytes());
        header[14..18].copy_from_slice(&40u32.to_le_bytes());
        header[18..22].copy_from_slice(&(width as i32).to_le_bytes());
        header[22..26].copy_from_slice(&(height as i32).to_le_bytes());
        header[26..28].copy_from_slice(&1u16.to_le_bytes());
        header[28..30].copy_from_slice(&24u16.to_le_bytes());
        header[34..38].copy_from_slice(&image_bytes.to_le_bytes());
        std::fs::write(path, header).unwrap();
    }

    struct TestComApartment;

    impl TestComApartment {
        fn initialize() -> Self {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

            let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            assert!(result.is_ok(), "CoInitializeEx failed: {result:?}");
            Self
        }
    }

    impl Drop for TestComApartment {
        fn drop(&mut self) {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
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
    fn scan_skips_oversized_files_before_image_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("oversized.jpg");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(crate::decode::MAX_IMAGE_FILE_BYTES + 1)
            .unwrap();

        let index =
            PhotoIndex::scan(&[dir.path().to_path_buf()], &["jpg".to_string()], false).unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn scan_skips_oversized_pixel_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pixel-bomb.bmp");
        write_bmp_header(&path, 10_001, 10_000);
        assert_eq!(image::image_dimensions(&path).unwrap(), (10_001, 10_000));
        assert!(crate::decode::validate_image_file(&path)
            .unwrap_err()
            .to_string()
            .contains("exceed maximum"));

        let index =
            PhotoIndex::scan(&[dir.path().to_path_buf()], &["bmp".to_string()], false).unwrap();

        assert!(index.is_empty());
    }

    #[test]
    fn scan_skips_malformed_wic_only_files() {
        let _com = TestComApartment::initialize();
        let dir = tempfile::tempdir().expect("tempdir");
        for extension in ["heic", "heif", "avif"] {
            std::fs::write(
                dir.path().join(format!("malformed.{extension}")),
                b"not an image",
            )
            .unwrap();
        }

        let extensions = ["heic", "heif", "avif"].map(str::to_string);
        let index = PhotoIndex::scan(&[dir.path().to_path_buf()], &extensions, false).unwrap();

        assert!(index.is_empty());
    }

    #[test]
    fn scan_skips_missing_and_duplicate_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_temp_jpg(dir.path(), "only-once.jpg");

        let roots = vec![
            dir.path().join("missing"),
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
        ];
        let index = PhotoIndex::scan(&roots, &["jpg".to_string()], true).unwrap();

        assert_eq!(index.photos.len(), 1);
    }

    #[test]
    fn scan_sources_rechecks_extensions_and_dimensions_for_each_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_temp_image(dir.path(), "small.jpg", 16, 16);
        make_temp_image(dir.path(), "small.png", 16, 16);

        let strict = source(dir.path().to_path_buf(), false, &["png"], 32, 32);
        let permissive = source(dir.path().to_path_buf(), false, &["jpg", "png"], 0, 0);

        for sources in [[strict.clone(), permissive.clone()], [permissive, strict]] {
            let index = PhotoIndex::scan_sources(&sources).unwrap();
            let mut names: Vec<_> = index
                .photos
                .iter()
                .map(|photo| {
                    photo
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();

            assert_eq!(names, ["small.jpg", "small.png"]);
        }
    }

    #[test]
    fn scan_sources_rechecks_recursion_for_each_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        make_temp_jpg(dir.path(), "root.jpg");
        make_temp_jpg(&nested, "nested.jpg");

        let shallow = source(dir.path().to_path_buf(), false, &["jpg"], 0, 0);
        let recursive = source(dir.path().to_path_buf(), true, &["jpg"], 0, 0);

        for sources in [[shallow.clone(), recursive.clone()], [recursive, shallow]] {
            let index = PhotoIndex::scan_sources(&sources).unwrap();

            assert_eq!(index.photos.len(), 2);
            assert!(index
                .photos
                .iter()
                .any(|photo| photo.path.ends_with("nested.jpg")));
        }
    }

    #[test]
    fn scan_sources_deduplicates_duplicate_and_alias_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_temp_jpg(dir.path(), "only-once.jpg");
        let root = dir.path().to_path_buf();
        let sources = vec![
            source(root.clone(), true, &["jpg"], 0, 0),
            source(root.clone(), true, &["jpg"], 0, 0),
            source(root.join("."), true, &["jpg"], 0, 0),
        ];

        let index = PhotoIndex::scan_sources(&sources).unwrap();

        assert_eq!(index.photos.len(), 1);
    }

    #[test]
    fn scan_sources_errors_when_every_configured_root_is_inaccessible() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sources = vec![
            source(dir.path().join("missing-a"), true, &["jpg"], 0, 0),
            source(dir.path().join("missing-b"), true, &["jpg"], 0, 0),
        ];

        let error = PhotoIndex::scan_sources(&sources).unwrap_err().to_string();

        assert!(error.contains("none of the 2 configured photo source roots"));
    }

    #[test]
    fn scan_sources_allows_mixed_accessible_and_inaccessible_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_temp_jpg(dir.path(), "accessible.jpg");
        let sources = vec![
            source(dir.path().join("missing"), true, &["jpg"], 0, 0),
            source(dir.path().to_path_buf(), true, &["jpg"], 0, 0),
        ];

        let index = PhotoIndex::scan_sources(&sources).unwrap();

        assert_eq!(index.photos.len(), 1);
    }

    #[test]
    fn scan_sources_allows_accessible_empty_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sources = [source(dir.path().to_path_buf(), true, &["jpg"], 0, 0)];

        let index = PhotoIndex::scan_sources(&sources).unwrap();

        assert!(index.is_empty());
    }

    #[test]
    fn persistent_cache_reuses_unchanged_validation_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        make_temp_jpg(dir.path(), "cached.jpg");
        let cache_path = dir.path().join("index-cache.json");
        let sources = [source(dir.path().to_path_buf(), false, &["jpg"], 0, 0)];

        let (first, first_hits, first_misses) = scan_with_test_cache(&sources, &cache_path);
        let (second, second_hits, second_misses) = scan_with_test_cache(&sources, &cache_path);

        assert_eq!((first_hits, first_misses), (0, 1));
        assert_eq!((second_hits, second_misses), (1, 0));
        assert_eq!(second.photos[0].hash, first.photos[0].hash);
        assert_eq!(second.photos[0].width, Some(16));
        assert_eq!(second.photos[0].height, Some(16));
    }

    #[test]
    fn persistent_cache_invalidates_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        let image_path = make_temp_jpg(dir.path(), "changed.jpg");
        let cache_path = dir.path().join("index-cache.json");
        let sources = [source(dir.path().to_path_buf(), false, &["jpg"], 0, 0)];
        let (first, _, _) = scan_with_test_cache(&sources, &cache_path);

        let image: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(24, 24, |_, _| Rgb([0u8, 0, 255]));
        image.save(&image_path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&image_path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2)),
            )
            .unwrap();

        let (second, hits, misses) = scan_with_test_cache(&sources, &cache_path);

        assert_eq!((hits, misses), (0, 1));
        assert_ne!(second.photos[0].hash, first.photos[0].hash);
        assert_eq!(second.photos[0].width, Some(24));
        assert_eq!(second.photos[0].height, Some(24));
    }

    #[test]
    fn persistent_cache_detects_same_size_rewrite_with_restored_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let image_path = make_temp_image(dir.path(), "changed.bmp", 16, 16);
        let cache_path = dir.path().join("index-cache.json");
        let sources = [source(dir.path().to_path_buf(), false, &["bmp"], 0, 0)];
        let (first, _, _) = scan_with_test_cache(&sources, &cache_path);
        let before = FileFingerprint::read(&image_path).unwrap();
        let modified = std::fs::metadata(&image_path).unwrap().modified().unwrap();

        let mut bytes = std::fs::read(&image_path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        std::fs::write(&image_path, bytes).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&image_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let after = FileFingerprint::read(&image_path).unwrap();

        assert_eq!(after.size, before.size);
        assert_eq!(after.creation_time, before.creation_time);
        assert_eq!(after.last_write_time, before.last_write_time);
        assert_eq!(after.volume_serial_number, before.volume_serial_number);
        assert_eq!(after.file_id, before.file_id);
        assert_ne!(after.change_time, before.change_time);

        let (second, hits, misses) = scan_with_test_cache(&sources, &cache_path);
        assert_eq!((hits, misses), (0, 1));
        assert_ne!(second.photos[0].hash, first.photos[0].hash);
    }

    #[test]
    fn cached_dimensions_still_obey_current_source_policy() {
        let dir = tempfile::tempdir().unwrap();
        make_temp_jpg(dir.path(), "small.jpg");
        let cache_path = dir.path().join("index-cache.json");
        let permissive = [source(dir.path().to_path_buf(), false, &["jpg"], 0, 0)];
        scan_with_test_cache(&permissive, &cache_path);
        let strict = [source(dir.path().to_path_buf(), false, &["jpg"], 32, 32)];

        let (index, hits, misses) = scan_with_test_cache(&strict, &cache_path);

        assert!(index.is_empty());
        assert_eq!((hits, misses), (1, 0));
    }

    #[test]
    fn malformed_persistent_cache_falls_back_to_a_full_scan() {
        let dir = tempfile::tempdir().unwrap();
        make_temp_jpg(dir.path(), "valid.jpg");
        let cache_path = dir.path().join("index-cache.json");
        std::fs::write(&cache_path, b"{ definitely not json").unwrap();
        let sources = [source(dir.path().to_path_buf(), false, &["jpg"], 0, 0)];

        let index = PhotoIndex::scan_sources_cached(&sources, &cache_path).unwrap();

        assert_eq!(index.len(), 1);
        assert!(load_index_cache(&cache_path).is_ok());
    }

    #[test]
    fn recursive_scan_stops_at_directory_cycles_when_links_are_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        make_temp_jpg(&nested, "cycle.jpg");
        let link = nested.join("back-to-root");

        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(dir.path(), &link).is_err() {
            return;
        }
        #[cfg(unix)]
        if std::os::unix::fs::symlink(dir.path(), &link).is_err() {
            return;
        }
        #[cfg(not(any(windows, unix)))]
        return;

        let index =
            PhotoIndex::scan(&[dir.path().to_path_buf()], &["jpg".to_string()], true).unwrap();

        assert_eq!(index.photos.len(), 1);
    }
}

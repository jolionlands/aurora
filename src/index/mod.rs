use anyhow::{bail, Context, Result};
use std::collections::{HashSet, VecDeque};
use std::io::BufReader;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use windows::Win32::Storage::FileSystem::FILE_FLAG_SEQUENTIAL_SCAN;

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

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

impl PhotoIndex {
    /// Scan one or more root directories and build the index.
    /// WIC-only formats require COM to be initialized on the current thread.
    pub fn scan(roots: &[PathBuf], extensions: &[String], recursive: bool) -> Result<Self> {
        let mut index = PhotoIndex::default();
        let exts: Vec<String> = extensions.iter().map(|e| e.to_lowercase()).collect();
        let mut visited_dirs = HashSet::new();
        let mut admitted_files = HashSet::new();

        for root in roots {
            collect_files(
                root,
                &exts,
                recursive,
                (0, 0),
                &mut visited_dirs,
                &mut admitted_files,
                &mut index,
            );
        }

        Ok(index)
    }

    /// Scan configured sources, preserving each source's extension, recursion,
    /// and minimum-dimension rules. WIC-only formats require COM to be
    /// initialized on the current thread.
    pub fn scan_sources(sources: &[SourceConfig]) -> Result<Self> {
        let mut index = PhotoIndex::default();
        let mut admitted_files = HashSet::new();
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
                &mut admitted_files,
                &mut index,
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

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn collect_files(
    dir: &Path,
    extensions: &[String],
    recursive: bool,
    min_dimensions: (u32, u32),
    visited_dirs: &mut HashSet<PathBuf>,
    admitted_files: &mut HashSet<PathBuf>,
    index: &mut PhotoIndex,
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
                collect_files(
                    &path,
                    extensions,
                    true,
                    min_dimensions,
                    visited_dirs,
                    admitted_files,
                    index,
                );
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
        if admitted_files.contains(&canonical_file) {
            continue;
        }

        match build_entry(&path, &meta, min_dimensions.0, min_dimensions.1) {
            Ok(Some(photo)) => {
                index.photos.push(photo);
                admitted_files.insert(canonical_file);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("skipping image {}: {}", path.display(), e);
            }
        }
    }

    true
}

fn build_entry(
    path: &Path,
    meta: &std::fs::Metadata,
    min_width: u32,
    min_height: u32,
) -> Result<Option<PhotoEntry>> {
    let size_bytes = meta.len();
    if size_bytes > crate::decode::MAX_IMAGE_FILE_BYTES {
        tracing::warn!(
            "skipping oversized image {}: {} bytes exceeds {} byte limit",
            path.display(),
            size_bytes,
            crate::decode::MAX_IMAGE_FILE_BYTES
        );
        return Ok(None);
    }
    let (verified_width, verified_height) = crate::decode::validate_image_file(path)
        .with_context(|| format!("invalid or unsupported image {}", path.display()))?;
    if verified_width < min_width || verified_height < min_height {
        return Ok(None);
    }

    let hash = hash_file(path)?;
    Ok(Some(PhotoEntry {
        path: path.to_path_buf(),
        width: Some(verified_width),
        height: Some(verified_height),
        hash,
        banned: false,
    }))
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

        let metadata = std::fs::metadata(&path).unwrap();
        assert!(build_entry(&path, &metadata, 0, 0).unwrap().is_none());
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

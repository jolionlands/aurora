/// Playlist support for aurora.
///
/// Data lives in `%APPDATA%\aurora\playlists.kdl`:
///
/// ```kdl
/// playlist "lakes" {
///     shuffle true
///     path "Acer/Wallpaper_Light_2560x1600.jpg"
///     path "wallhaven-z8pyky.jpg"
/// }
///
/// playlist "screenshots" {
///     shuffle false
///     path "Screenshots/Screenshot 2026-05-25 174408.png"
/// }
///
/// active "lakes"
/// ```
///
/// Paths inside a playlist are stored as-written (typically relative to the
/// configured source root).  The daemon resolves them against the source root
/// at pick time; non-existent files are silently skipped.
use anyhow::{Context, Result};
use rand::Rng;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Playlist {
    pub name: String,
    pub shuffle: bool,
    /// Paths as stored in the KDL (may be relative or absolute).
    pub paths: Vec<String>,
    /// Per-path tags. Keys match entries in `paths` exactly as stored.
    pub tags: HashMap<String, Vec<String>>,
    /// Theme/style tags, e.g. cyberpunk, nature, minimal, vaporwave.
    pub themes: HashMap<String, Vec<String>>,
    /// Content/subject tags, e.g. mountain, city, character, abstract.
    pub content: HashMap<String, Vec<String>>,
    /// Color/palette tags, e.g. blue, neon, monochrome, warm.
    pub colors: HashMap<String, Vec<String>>,
    /// Source/collection tags, e.g. wallhaven, generated, camera-roll.
    pub sources: HashMap<String, Vec<String>>,
    /// Medium/type tags, e.g. anime, real-photo, illustration, screenshot.
    pub media: HashMap<String, Vec<String>>,
    /// Safety tags, e.g. sfw, nsfw, lewd.
    pub safety: HashMap<String, Vec<String>>,
    /// Franchise/fandom tags, e.g. naruto, persona, one-piece.
    pub franchises: HashMap<String, Vec<String>>,
    /// Character tags, e.g. naruto-uzumaki, hatsune-miku.
    pub characters: HashMap<String, Vec<String>>,
    /// Future/custom tag groups. Outer key is the group name, inner key is path.
    pub custom_tags: HashMap<String, HashMap<String, Vec<String>>>,
    /// Per-path star ratings, 0-5. Higher ratings are picked more often.
    pub ratings: HashMap<String, u8>,
    /// Per-path frequency weights. Values below 1 are normalized to 1.
    pub frequencies: HashMap<String, u32>,
}

#[derive(Debug, Clone, Default)]
pub struct PlaylistStore {
    /// Ordered list so `list` output is stable.
    pub playlists: Vec<Playlist>,
    /// Name of the currently active playlist, if any.
    pub active: Option<String>,
}

impl PlaylistStore {
    // -----------------------------------------------------------------------
    // Lookup helpers
    // -----------------------------------------------------------------------

    pub fn get(&self, name: &str) -> Option<&Playlist> {
        self.playlists.iter().find(|p| p.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Playlist> {
        self.playlists.iter_mut().find(|p| p.name == name)
    }

    pub fn active_playlist(&self) -> Option<&Playlist> {
        self.active.as_deref().and_then(|n| self.get(n))
    }

    // -----------------------------------------------------------------------
    // Mutations (all return Ok so callers can atomically persist afterward)
    // -----------------------------------------------------------------------

    /// Create an empty playlist.  Returns `Err` if the name already exists.
    pub fn create(&mut self, name: &str) -> Result<()> {
        if self.get(name).is_some() {
            anyhow::bail!("playlist '{}' already exists", name);
        }
        self.playlists.push(Playlist {
            name: name.to_string(),
            shuffle: false,
            paths: Vec::new(),
            tags: HashMap::new(),
            themes: HashMap::new(),
            content: HashMap::new(),
            colors: HashMap::new(),
            sources: HashMap::new(),
            media: HashMap::new(),
            safety: HashMap::new(),
            franchises: HashMap::new(),
            characters: HashMap::new(),
            custom_tags: HashMap::new(),
            ratings: HashMap::new(),
            frequencies: HashMap::new(),
        });
        Ok(())
    }

    /// Add `path` to the named playlist (appends; duplicates are allowed so the
    /// user can weight a file by repeating it, consistent with the spec).
    pub fn add_path(&mut self, name: &str, path: &str) -> Result<()> {
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        pl.paths.push(path.to_string());
        Ok(())
    }

    /// Remove the first occurrence of `path` from the named playlist.
    pub fn remove_path(&mut self, name: &str, path: &str) -> Result<()> {
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        if let Some(pos) = pl.paths.iter().position(|p| p == path) {
            pl.paths.remove(pos);
            if !pl.paths.iter().any(|p| p == path) {
                pl.tags.remove(path);
                pl.themes.remove(path);
                pl.content.remove(path);
                pl.colors.remove(path);
                pl.sources.remove(path);
                pl.media.remove(path);
                pl.safety.remove(path);
                pl.franchises.remove(path);
                pl.characters.remove(path);
                for group in pl.custom_tags.values_mut() {
                    group.remove(path);
                }
                pl.ratings.remove(path);
                pl.frequencies.remove(path);
            }
            Ok(())
        } else {
            anyhow::bail!("path '{}' not in playlist '{}'", path, name);
        }
    }

    pub fn set_tags(&mut self, name: &str, path: &str, tags: Vec<String>) -> Result<()> {
        self.set_tag_group(name, path, "general", tags)
    }

    pub fn set_tag_group(
        &mut self,
        name: &str,
        path: &str,
        kind: &str,
        tags: Vec<String>,
    ) -> Result<()> {
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        ensure_path_exists(pl, path)?;
        let tags: Vec<String> = tags
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        set_group_map(pl, path, kind, tags)?;
        Ok(())
    }

    pub fn set_rating(&mut self, name: &str, path: &str, rating: u8) -> Result<()> {
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        ensure_path_exists(pl, path)?;
        pl.ratings.insert(path.to_string(), rating.min(5));
        Ok(())
    }

    pub fn set_frequency(&mut self, name: &str, path: &str, frequency: u32) -> Result<()> {
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        ensure_path_exists(pl, path)?;
        pl.frequencies.insert(path.to_string(), frequency.max(1));
        Ok(())
    }

    /// Set the active playlist.  Returns `Err` if the name does not exist.
    pub fn activate(&mut self, name: &str) -> Result<()> {
        if self.get(name).is_none() {
            anyhow::bail!("playlist '{}' not found", name);
        }
        self.active = Some(name.to_string());
        Ok(())
    }

    /// Clear the active playlist (returns to full-index rotation).
    pub fn deactivate(&mut self) {
        self.active = None;
    }

    /// Delete a playlist.  If it was active, clears the active marker.
    pub fn delete(&mut self, name: &str) -> Result<()> {
        let pos = self
            .playlists
            .iter()
            .position(|p| p.name == name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        self.playlists.remove(pos);
        if self.active.as_deref() == Some(name) {
            self.active = None;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Picker
    // -----------------------------------------------------------------------

    /// Pick an absolute path from the active playlist, resolving relative paths
    /// against `source_root`.  Files that do not exist on disk are silently
    /// skipped.  Returns `None` if the playlist is empty or all files are missing.
    pub fn pick(
        &self,
        source_root: Option<&Path>,
        // Sequential cursor: maps playlist_name -> next weighted slot. Updated in-place.
        cursor: &mut HashMap<String, usize>,
        recent_window: usize,
        recent_paths: &VecDeque<PathBuf>,
    ) -> Option<PathBuf> {
        let roots: Vec<&Path> = source_root.into_iter().collect();
        self.pick_from_roots(&roots, cursor, recent_window, recent_paths)
    }

    /// Pick from a playlist, resolving relative paths against every configured
    /// source root until an existing file is found.
    pub fn pick_from_roots(
        &self,
        source_roots: &[&Path],
        cursor: &mut HashMap<String, usize>,
        recent_window: usize,
        recent_paths: &VecDeque<PathBuf>,
    ) -> Option<PathBuf> {
        let pl = self.active_playlist()?;

        // Resolve all existing paths with their configured weights.
        let existing: Vec<(String, PathBuf, u64)> = pl
            .paths
            .iter()
            .filter_map(|s| {
                let p = PathBuf::from(s);
                let resolved = if p.is_absolute() || source_roots.is_empty() {
                    p
                } else {
                    source_roots
                        .iter()
                        .map(|root| root.join(&p))
                        .find(|candidate| candidate.is_file())?
                };
                if !resolved.is_file() {
                    return None;
                }
                let frequency = pl.frequencies.get(s).copied().unwrap_or(1).max(1) as u64;
                let rating_weight = pl.ratings.get(s).map(|r| *r as u64 + 1).unwrap_or(1);
                Some((
                    s.clone(),
                    resolved,
                    frequency.saturating_mul(rating_weight).max(1),
                ))
            })
            .collect();

        if existing.is_empty() {
            return None;
        }

        if pl.shuffle {
            let recent_set: std::collections::HashSet<&PathBuf> =
                recent_paths.iter().rev().take(recent_window).collect();
            let candidates: Vec<usize> = existing
                .iter()
                .enumerate()
                .filter(|(_, (_, p, _))| !recent_set.contains(p))
                .map(|(i, _)| i)
                .collect();
            let pool: Vec<usize> = if candidates.is_empty() {
                (0..existing.len()).collect()
            } else {
                candidates
            };
            let total: u64 = pool
                .iter()
                .map(|i| existing[*i].2)
                .fold(0, u64::saturating_add);
            let fallback = existing[*pool.last()?].1.clone();
            let mut pick = rand::thread_rng().gen_range(0..total);
            for idx in pool {
                let weight = existing[idx].2;
                if pick < weight {
                    return Some(existing[idx].1.clone());
                }
                pick -= weight;
            }
            Some(fallback)
        } else {
            let recent_set: std::collections::HashSet<&PathBuf> =
                recent_paths.iter().rev().take(recent_window).collect();
            let all: Vec<usize> = (0..existing.len()).collect();
            let candidates: Vec<usize> = all
                .iter()
                .copied()
                .filter(|i| !recent_set.contains(&existing[*i].1))
                .collect();
            let pool = if candidates.is_empty() {
                all
            } else {
                candidates
            };

            // Avoid expanding weights into a potentially enormous temporary vector.
            let cur = cursor.entry(pl.name.clone()).or_insert(0);
            let total: u64 = pool
                .iter()
                .map(|i| existing[*i].2)
                .fold(0, u64::saturating_add);
            if total == 0 {
                return None;
            }
            let mut slot = (*cur as u64) % total;
            let next = slot + 1;
            *cur = if next >= total {
                0
            } else {
                next.min(usize::MAX as u64) as usize
            };
            for idx in &pool {
                let weight = existing[*idx].2;
                if slot < weight {
                    return Some(existing[*idx].1.clone());
                }
                slot -= weight;
            }
            Some(existing[*pool.last()?].1.clone())
        }
    }
}

// ---------------------------------------------------------------------------
// Persistence path
// ---------------------------------------------------------------------------

pub fn default_playlists_path() -> PathBuf {
    let mut p = appdata_aurora_dir();
    p.push("playlists.kdl");
    p
}

fn appdata_aurora_dir() -> PathBuf {
    let base = if let Some(p) = std::env::var_os("APPDATA") {
        PathBuf::from(p)
    } else if let Some(p) = std::env::var_os("USERPROFILE") {
        PathBuf::from(p).join("AppData").join("Roaming")
    } else {
        PathBuf::from(".")
    };
    base.join("aurora")
}

// ---------------------------------------------------------------------------
// KDL parser
// ---------------------------------------------------------------------------

/// Parse a `playlists.kdl` file into a `PlaylistStore`.
/// Uses the same BOM-tolerant, hand-rolled approach as `config/parse.rs`.
pub fn parse_playlists(input: &str) -> Result<PlaylistStore> {
    let input = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let mut store = PlaylistStore::default();

    // Parser state
    let mut in_playlist: Option<Playlist> = None;

    for raw in input.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if line == "}" {
            // Close current playlist block.
            if let Some(pl) = in_playlist.take() {
                store.playlists.push(pl);
            }
            continue;
        }

        // Section open: `playlist "name" {`
        if let Some(rest) = line.strip_prefix("playlist") {
            if let Some(body) = rest.strip_suffix('{') {
                let body = body.trim();
                if let Some(name) = extract_quoted(body) {
                    in_playlist = Some(Playlist {
                        name,
                        shuffle: false,
                        paths: Vec::new(),
                        tags: HashMap::new(),
                        themes: HashMap::new(),
                        content: HashMap::new(),
                        colors: HashMap::new(),
                        sources: HashMap::new(),
                        media: HashMap::new(),
                        safety: HashMap::new(),
                        franchises: HashMap::new(),
                        characters: HashMap::new(),
                        custom_tags: HashMap::new(),
                        ratings: HashMap::new(),
                        frequencies: HashMap::new(),
                    });
                }
                continue;
            }
        }

        // Top-level `active "name"` directive.
        if let Some(rest) = line.strip_prefix("active") {
            let rest = rest.trim();
            if let Some(name) = extract_quoted(rest) {
                store.active = Some(name);
            }
            continue;
        }

        // Inside a playlist block.
        if let Some(ref mut pl) = in_playlist {
            // `shuffle true/false`
            if let Some(val) = line.strip_prefix("shuffle") {
                pl.shuffle = parse_bool(val.trim());
                continue;
            }
            // `path "..."`
            if let Some(rest) = line.strip_prefix("path") {
                let rest = rest.trim();
                if let Some(p) = extract_quoted(rest) {
                    pl.paths.push(p);
                } else if !rest.is_empty() {
                    // Bare (unquoted) path token.
                    pl.paths.push(rest.to_string());
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("tag") {
                let quoted = extract_all_quoted(rest.trim());
                match quoted.as_slice() {
                    [path, tag] => {
                        let _ = push_group_tag(pl, path, "general", tag);
                    }
                    [path, kind, tag] => {
                        let _ = push_group_tag(pl, path, kind, tag);
                    }
                    _ => {}
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("theme") {
                if let Some((path, tag)) = extract_two_quoted(rest.trim()) {
                    let _ = push_group_tag(pl, &path, "theme", &tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("content") {
                if let Some((path, tag)) = extract_two_quoted(rest.trim()) {
                    let _ = push_group_tag(pl, &path, "content", &tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("color") {
                if let Some((path, tag)) = extract_two_quoted(rest.trim()) {
                    let _ = push_group_tag(pl, &path, "color", &tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("source") {
                if let Some((path, tag)) = extract_two_quoted(rest.trim()) {
                    let _ = push_group_tag(pl, &path, "source", &tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("medium") {
                if let Some((path, tag)) = extract_two_quoted(rest.trim()) {
                    let _ = push_group_tag(pl, &path, "medium", &tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("safety") {
                if let Some((path, tag)) = extract_two_quoted(rest.trim()) {
                    let _ = push_group_tag(pl, &path, "safety", &tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("franchise") {
                if let Some((path, tag)) = extract_two_quoted(rest.trim()) {
                    let _ = push_group_tag(pl, &path, "franchise", &tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("character") {
                if let Some((path, tag)) = extract_two_quoted(rest.trim()) {
                    let _ = push_group_tag(pl, &path, "character", &tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("facet") {
                let quoted = extract_all_quoted(rest.trim());
                if let [path, kind, tag] = quoted.as_slice() {
                    let _ = push_group_tag(pl, path, kind, tag);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("rating") {
                if let Some((path, value)) = extract_quoted_and_tail(rest.trim()) {
                    if let Ok(rating) = value.trim().parse::<u8>() {
                        pl.ratings.insert(path, rating.min(5));
                    }
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("frequency") {
                if let Some((path, value)) = extract_quoted_and_tail(rest.trim()) {
                    if let Ok(frequency) = value.trim().parse::<u32>() {
                        pl.frequencies.insert(path, frequency.max(1));
                    }
                }
                continue;
            }
            // Unknown keys inside a playlist block — silently ignore.
        }
    }

    Ok(store)
}

// ---------------------------------------------------------------------------
// KDL serialiser
// ---------------------------------------------------------------------------

/// Serialise a `PlaylistStore` to KDL text.
pub fn serialize_playlists(store: &PlaylistStore) -> String {
    let mut out = String::new();

    for pl in &store.playlists {
        out.push_str(&format!("playlist \"{}\" {{\n", escape_kdl(&pl.name)));
        out.push_str(&format!(
            "    shuffle {}\n",
            if pl.shuffle { "true" } else { "false" }
        ));
        for path in &pl.paths {
            out.push_str(&format!("    path \"{}\"\n", escape_kdl(path)));
        }
        for path in &pl.paths {
            if let Some(tags) = pl.tags.get(path) {
                for tag in tags {
                    out.push_str(&format!(
                        "    tag \"{}\" \"{}\"\n",
                        escape_kdl(path),
                        escape_kdl(tag)
                    ));
                }
            }
            write_tag_lines(&mut out, "theme", path, pl.themes.get(path));
            write_tag_lines(&mut out, "content", path, pl.content.get(path));
            write_tag_lines(&mut out, "color", path, pl.colors.get(path));
            write_tag_lines(&mut out, "source", path, pl.sources.get(path));
            write_tag_lines(&mut out, "medium", path, pl.media.get(path));
            write_tag_lines(&mut out, "safety", path, pl.safety.get(path));
            write_tag_lines(&mut out, "franchise", path, pl.franchises.get(path));
            write_tag_lines(&mut out, "character", path, pl.characters.get(path));
            for (kind, group) in &pl.custom_tags {
                if let Some(tags) = group.get(path) {
                    for tag in tags {
                        out.push_str(&format!(
                            "    facet \"{}\" \"{}\" \"{}\"\n",
                            escape_kdl(path),
                            escape_kdl(kind),
                            escape_kdl(tag)
                        ));
                    }
                }
            }
            if let Some(rating) = pl.ratings.get(path) {
                out.push_str(&format!(
                    "    rating \"{}\" {}\n",
                    escape_kdl(path),
                    (*rating).min(5)
                ));
            }
            if let Some(frequency) = pl.frequencies.get(path) {
                out.push_str(&format!(
                    "    frequency \"{}\" {}\n",
                    escape_kdl(path),
                    (*frequency).max(1)
                ));
            }
        }
        out.push_str("}\n\n");
    }

    if let Some(ref active) = store.active {
        out.push_str(&format!("active \"{}\"\n", escape_kdl(active)));
    }

    out
}

// ---------------------------------------------------------------------------
// Atomic write
// ---------------------------------------------------------------------------

/// Write `store` to `path` atomically via a `.tmp` sibling + rename.
pub fn persist_playlists(store: &PlaylistStore, path: &Path) -> Result<()> {
    let content = serialize_playlists(store);

    // Ensure the parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create playlists dir {}", parent.display()))?;
    }

    let tmp = path.with_extension("kdl.tmp");
    std::fs::write(&tmp, content.as_bytes())
        .with_context(|| format!("write playlists tmp {}", tmp.display()))?;
    replace_file(&tmp, path)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn replace_file(tmp: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let tmp_wide: Vec<u16> = tmp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(tmp_wide.as_ptr()),
            PCWSTR::from_raw(path_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
    }
}

#[cfg(not(target_os = "windows"))]
fn replace_file(tmp: &Path, path: &Path) -> Result<()> {
    std::fs::rename(tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
}

/// Load the playlist store from disk (creates an empty one if the file does not exist).
pub fn load_playlists(path: &Path) -> Result<PlaylistStore> {
    if !path.exists() {
        return Ok(PlaylistStore::default());
    }
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("read playlists {}", path.display()))?;
    parse_playlists(&src)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn strip_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = Some(byte);
        } else if byte == b'/' && bytes.get(i + 1) == Some(&b'/') {
            return s[..i].trim_end();
        }
        i += 1;
    }
    s
}

fn quoted_span(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|b| *b == b'"' || *b == b'\'')?;
    let quote = bytes[start];
    let mut escaped = false;
    for (i, byte) in bytes.iter().enumerate().skip(start + 1) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == quote {
            return Some((start, i));
        }
    }
    None
}

/// Extract the first double- or single-quoted string from `s`.
fn extract_quoted(s: &str) -> Option<String> {
    let (start, end) = quoted_span(s)?;
    Some(unescape_kdl(&s[start + 1..end]))
}

fn extract_quoted_and_tail(s: &str) -> Option<(String, String)> {
    let (start, end) = quoted_span(s)?;
    let path = unescape_kdl(&s[start + 1..end]);
    let tail = s[end + 1..].trim().to_string();
    Some((path, tail))
}

fn extract_two_quoted(s: &str) -> Option<(String, String)> {
    let (first, tail) = extract_quoted_and_tail(s)?;
    let second = extract_quoted(&tail)?;
    Some((first, second))
}

fn extract_all_quoted(s: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut offset = 0;
    while offset < s.len() {
        let Some((start, end)) = quoted_span(&s[offset..]) else {
            break;
        };
        let start = offset + start;
        let end = offset + end;
        values.push(unescape_kdl(&s[start + 1..end]));
        offset = end + 1;
    }
    values
}

fn ensure_path_exists(pl: &Playlist, path: &str) -> Result<()> {
    if pl.paths.iter().any(|p| p == path) {
        Ok(())
    } else {
        anyhow::bail!("path '{}' not in playlist '{}'", path, pl.name)
    }
}

fn normalized_tag_kind(kind: &str) -> Option<&'static str> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "" | "general" | "tag" | "tags" => Some("general"),
        "theme" | "themes" | "style" | "styles" => Some("theme"),
        "content" | "contents" | "subject" | "subjects" => Some("content"),
        "color" | "colors" | "colour" | "colours" | "palette" | "palettes" => Some("color"),
        "source" | "sources" | "collection" | "collections" => Some("source"),
        "medium" | "media" | "type" | "types" | "photo" | "photos" | "real" | "anime"
        | "illustration" | "illustrations" | "screenshot" | "screenshots" => Some("medium"),
        "safety" | "rating" | "ratings" | "sfw" | "nsfw" | "lewd" => Some("safety"),
        "franchise" | "franchises" | "fandom" | "fandoms" | "series" | "show" | "game" => {
            Some("franchise")
        }
        "character" | "characters" | "person" | "people" => Some("character"),
        _ => None,
    }
}

fn normalize_custom_kind(kind: &str) -> String {
    kind.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn group_map_mut<'a>(
    pl: &'a mut Playlist,
    kind: &str,
) -> Result<&'a mut HashMap<String, Vec<String>>> {
    match normalized_tag_kind(kind) {
        Some("general") => Ok(&mut pl.tags),
        Some("theme") => Ok(&mut pl.themes),
        Some("content") => Ok(&mut pl.content),
        Some("color") => Ok(&mut pl.colors),
        Some("source") => Ok(&mut pl.sources),
        Some("medium") => Ok(&mut pl.media),
        Some("safety") => Ok(&mut pl.safety),
        Some("franchise") => Ok(&mut pl.franchises),
        Some("character") => Ok(&mut pl.characters),
        _ => anyhow::bail!(
            "unknown tag kind '{}'; expected general, theme, content, color, source, medium, safety, franchise, or character",
            kind
        ),
    }
}

fn set_group_map(pl: &mut Playlist, path: &str, kind: &str, tags: Vec<String>) -> Result<()> {
    let map = match group_map_mut(pl, kind) {
        Ok(map) => map,
        Err(_) => {
            let custom_kind = normalize_custom_kind(kind);
            if custom_kind.is_empty() {
                anyhow::bail!("empty tag kind");
            }
            pl.custom_tags.entry(custom_kind).or_default()
        }
    };
    if tags.is_empty() {
        map.remove(path);
    } else {
        map.insert(path.to_string(), tags);
    }
    Ok(())
}

fn push_group_tag(pl: &mut Playlist, path: &str, kind: &str, tag: &str) -> Result<()> {
    let tag = tag.trim();
    if tag.is_empty() {
        return Ok(());
    }
    let map = match group_map_mut(pl, kind) {
        Ok(map) => map,
        Err(_) => {
            let custom_kind = normalize_custom_kind(kind);
            if custom_kind.is_empty() {
                anyhow::bail!("empty tag kind");
            }
            pl.custom_tags.entry(custom_kind).or_default()
        }
    };
    map.entry(path.to_string())
        .or_default()
        .push(tag.to_string());
    Ok(())
}

fn write_tag_lines(out: &mut String, key: &str, path: &str, tags: Option<&Vec<String>>) {
    if let Some(tags) = tags {
        for tag in tags {
            out.push_str(&format!(
                "    {} \"{}\" \"{}\"\n",
                key,
                escape_kdl(path),
                escape_kdl(tag)
            ));
        }
    }
}

fn parse_bool(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "true" | "1" | "yes" | "on")
}

/// Escape a string for inclusion inside KDL double-quotes.
fn escape_kdl(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Unescape `\n`, `\t`, `\\`, `\"` sequences inside a quoted KDL string.
fn unescape_kdl(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
playlist "lakes" {
    shuffle true
    path "Acer/Wallpaper_Light_2560x1600.jpg"
    path "wallhaven-z8pyky.jpg"
}

playlist "screenshots" {
    shuffle false
    path "Screenshots/Screenshot 2026-05-25 174408.png"
    path "Screenshots/Screenshot 2026-05-25 175310.png"
}

active "lakes"
"#;

    #[test]
    fn test_parse_roundtrip() {
        let store = parse_playlists(SAMPLE).expect("parse");
        assert_eq!(store.playlists.len(), 2);
        assert_eq!(store.playlists[0].name, "lakes");
        assert!(store.playlists[0].shuffle);
        assert_eq!(store.playlists[0].paths.len(), 2);
        assert_eq!(store.playlists[1].name, "screenshots");
        assert!(!store.playlists[1].shuffle);
        assert_eq!(store.active, Some("lakes".to_string()));
    }

    #[test]
    fn test_parse_strips_bom() {
        let bom_src = format!("\u{FEFF}{}", SAMPLE);
        let store = parse_playlists(&bom_src).expect("BOM parse");
        assert_eq!(store.playlists.len(), 2);
    }

    #[test]
    fn test_parse_preserves_escaped_quotes_and_comment_markers() {
        let src = r#"
playlist "special" {
    path "folder//wall\\\"paper.jpg" // this is a comment
    tag "folder//wall\\\"paper.jpg" "calm//blue"
}
"#;
        let store = parse_playlists(src).expect("parse escaped path");
        let pl = store.get("special").expect("playlist");
        assert_eq!(pl.paths, vec![r#"folder//wall\"paper.jpg"#.to_string()]);
        assert_eq!(
            pl.tags.get(r#"folder//wall\"paper.jpg"#).unwrap(),
            &vec!["calm//blue".to_string()]
        );
    }

    #[test]
    fn test_serialize_parse_identity() {
        let store = parse_playlists(SAMPLE).expect("parse");
        let kdl = serialize_playlists(&store);
        let store2 = parse_playlists(&kdl).expect("re-parse");
        assert_eq!(store2.playlists.len(), store.playlists.len());
        assert_eq!(store2.active, store.active);
        for (a, b) in store.playlists.iter().zip(store2.playlists.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.shuffle, b.shuffle);
            assert_eq!(a.paths, b.paths);
        }
    }

    #[test]
    fn test_create_add_remove() {
        let mut store = PlaylistStore::default();
        store.create("test").unwrap();
        store.add_path("test", "foo.jpg").unwrap();
        store.add_path("test", "bar.jpg").unwrap();
        assert_eq!(store.get("test").unwrap().paths.len(), 2);
        store.remove_path("test", "foo.jpg").unwrap();
        assert_eq!(store.get("test").unwrap().paths, vec!["bar.jpg"]);
    }

    #[test]
    fn test_activate_deactivate_delete() {
        let mut store = PlaylistStore::default();
        store.create("alpha").unwrap();
        store.create("beta").unwrap();
        store.activate("alpha").unwrap();
        assert_eq!(store.active, Some("alpha".to_string()));
        store.deactivate();
        assert_eq!(store.active, None);
        store.activate("beta").unwrap();
        store.delete("beta").unwrap();
        assert_eq!(store.active, None);
        assert!(store.get("beta").is_none());
    }

    #[test]
    fn test_duplicate_name_rejected() {
        let mut store = PlaylistStore::default();
        store.create("x").unwrap();
        assert!(store.create("x").is_err());
    }

    #[test]
    fn test_pick_sequential_wraps() {
        let mut store = PlaylistStore::default();
        store.create("seq").unwrap();

        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        std::fs::write(&a, b"data").unwrap();
        std::fs::write(&b, b"data").unwrap();

        store.add_path("seq", &a.to_string_lossy()).unwrap();
        store.add_path("seq", &b.to_string_lossy()).unwrap();
        store.activate("seq").unwrap();

        let mut cursor = HashMap::new();
        let recent = VecDeque::new();
        let p1 = store.pick(None, &mut cursor, 0, &recent).unwrap();
        let p2 = store.pick(None, &mut cursor, 0, &recent).unwrap();
        let p3 = store.pick(None, &mut cursor, 0, &recent).unwrap(); // wraps back to first
        assert_ne!(p1, p2);
        assert_eq!(p1, p3);
    }

    #[test]
    fn test_pick_uses_all_roots_and_avoids_recent_sequential_items() {
        let mut store = PlaylistStore::default();
        store.create("seq").unwrap();
        store.activate("seq").unwrap();

        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        std::fs::write(root_a.path().join("first.jpg"), b"a").unwrap();
        std::fs::write(root_b.path().join("second.jpg"), b"b").unwrap();
        store.add_path("seq", "first.jpg").unwrap();
        store.add_path("seq", "second.jpg").unwrap();
        store.set_frequency("seq", "first.jpg", u32::MAX).unwrap();

        let roots = vec![root_a.path(), root_b.path()];
        let mut cursor = HashMap::new();
        let mut recent = VecDeque::new();
        let first = store
            .pick_from_roots(&roots, &mut cursor, 0, &recent)
            .unwrap();
        assert_eq!(first, root_a.path().join("first.jpg"));

        recent.push_back(first);
        let second = store
            .pick_from_roots(&roots, &mut cursor, 1, &recent)
            .unwrap();
        assert_eq!(second, root_b.path().join("second.jpg"));
    }

    #[test]
    fn test_metadata_roundtrip() {
        let src = r#"
playlist "focus" {
    shuffle true
    path "a.jpg"
    tag "a.jpg" "calm"
    tag "a.jpg" "blue"
    tag "a.jpg" "theme" "minimal"
    content "a.jpg" "mountain"
    color "a.jpg" "cyan"
    source "a.jpg" "wallhaven"
    medium "a.jpg" "anime"
    safety "a.jpg" "nsfw"
    franchise "a.jpg" "naruto"
    character "a.jpg" "naruto-uzumaki"
    facet "a.jpg" "artist" "kishimoto"
    rating "a.jpg" 4
    frequency "a.jpg" 3
}
"#;
        let store = parse_playlists(src).expect("parse");
        let pl = store.get("focus").expect("playlist");
        assert_eq!(
            pl.tags.get("a.jpg").unwrap(),
            &vec!["calm".to_string(), "blue".to_string()]
        );
        assert_eq!(
            pl.themes.get("a.jpg").unwrap(),
            &vec!["minimal".to_string()]
        );
        assert_eq!(
            pl.content.get("a.jpg").unwrap(),
            &vec!["mountain".to_string()]
        );
        assert_eq!(pl.colors.get("a.jpg").unwrap(), &vec!["cyan".to_string()]);
        assert_eq!(
            pl.sources.get("a.jpg").unwrap(),
            &vec!["wallhaven".to_string()]
        );
        assert_eq!(pl.media.get("a.jpg").unwrap(), &vec!["anime".to_string()]);
        assert_eq!(pl.safety.get("a.jpg").unwrap(), &vec!["nsfw".to_string()]);
        assert_eq!(
            pl.franchises.get("a.jpg").unwrap(),
            &vec!["naruto".to_string()]
        );
        assert_eq!(
            pl.characters.get("a.jpg").unwrap(),
            &vec!["naruto-uzumaki".to_string()]
        );
        assert_eq!(
            pl.custom_tags.get("artist").unwrap().get("a.jpg").unwrap(),
            &vec!["kishimoto".to_string()]
        );
        assert_eq!(pl.ratings.get("a.jpg"), Some(&4));
        assert_eq!(pl.frequencies.get("a.jpg"), Some(&3));

        let serialized = serialize_playlists(&store);
        let reparsed = parse_playlists(&serialized).expect("reparse");
        let pl = reparsed.get("focus").expect("playlist");
        assert_eq!(pl.tags.get("a.jpg").unwrap().len(), 2);
        assert_eq!(
            pl.themes.get("a.jpg").unwrap(),
            &vec!["minimal".to_string()]
        );
        assert_eq!(
            pl.content.get("a.jpg").unwrap(),
            &vec!["mountain".to_string()]
        );
        assert_eq!(pl.colors.get("a.jpg").unwrap(), &vec!["cyan".to_string()]);
        assert_eq!(
            pl.sources.get("a.jpg").unwrap(),
            &vec!["wallhaven".to_string()]
        );
        assert_eq!(pl.media.get("a.jpg").unwrap(), &vec!["anime".to_string()]);
        assert_eq!(pl.safety.get("a.jpg").unwrap(), &vec!["nsfw".to_string()]);
        assert_eq!(
            pl.franchises.get("a.jpg").unwrap(),
            &vec!["naruto".to_string()]
        );
        assert_eq!(
            pl.characters.get("a.jpg").unwrap(),
            &vec!["naruto-uzumaki".to_string()]
        );
        assert_eq!(
            pl.custom_tags.get("artist").unwrap().get("a.jpg").unwrap(),
            &vec!["kishimoto".to_string()]
        );
        assert_eq!(pl.ratings.get("a.jpg"), Some(&4));
        assert_eq!(pl.frequencies.get("a.jpg"), Some(&3));
    }

    #[test]
    fn test_atomic_persist_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("playlists.kdl");

        let mut store = PlaylistStore::default();
        store.create("x").unwrap();
        store.add_path("x", "photo.jpg").unwrap();
        store.activate("x").unwrap();

        persist_playlists(&store, &path).unwrap();
        assert!(path.exists());

        store.create("y").unwrap();
        persist_playlists(&store, &path).unwrap();

        let loaded = load_playlists(&path).unwrap();
        assert_eq!(loaded.playlists.len(), 2);
        assert_eq!(loaded.active, Some("x".to_string()));
    }
}

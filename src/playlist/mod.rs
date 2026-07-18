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
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
    /// Tag kind → stored path → tags, including built-in and custom groups.
    pub tag_groups: BTreeMap<String, HashMap<String, Vec<String>>>,
    /// Per-path star ratings, 0-5. Higher ratings are picked more often.
    pub ratings: HashMap<String, u8>,
    /// Per-path positive frequency weights.
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
        validate_playlist_name(name)?;
        if self.get(name).is_some() {
            anyhow::bail!("playlist '{}' already exists", name);
        }
        self.playlists.push(Playlist {
            name: name.to_string(),
            ..Playlist::default()
        });
        Ok(())
    }

    /// Add `path` to the named playlist. Existing paths are left unchanged;
    /// frequency is the supported weighting mechanism.
    pub fn add_path(&mut self, name: &str, path: &str) -> Result<()> {
        validate_playlist_path(path)?;
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        if !pl.paths.iter().any(|stored| stored == path) {
            pl.paths.push(path.to_string());
        }
        Ok(())
    }

    /// Remove `path` from the named playlist.
    pub fn remove_path(&mut self, name: &str, path: &str) -> Result<()> {
        validate_playlist_path(path)?;
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        if let Some(pos) = pl.paths.iter().position(|p| p == path) {
            pl.paths.remove(pos);
            if !pl.paths.iter().any(|p| p == path) {
                clear_playlist_path_metadata(pl, path);
            }
            Ok(())
        } else {
            anyhow::bail!("path '{}' not in playlist '{}'", path, name);
        }
    }

    pub fn clear_path_metadata(&mut self, name: &str, path: &str) -> Result<()> {
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        ensure_path_exists(pl, path)?;
        clear_playlist_path_metadata(pl, path);
        Ok(())
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
        let mut tags: Vec<String> = tags
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        dedupe_strings(&mut tags);
        set_group_map(pl, path, kind, tags)?;
        Ok(())
    }

    pub fn set_rating(&mut self, name: &str, path: &str, rating: u8) -> Result<()> {
        if rating > 5 {
            anyhow::bail!("rating must be between 0 and 5");
        }
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        ensure_path_exists(pl, path)?;
        pl.ratings.insert(path.to_string(), rating);
        Ok(())
    }

    pub fn set_frequency(&mut self, name: &str, path: &str, frequency: u32) -> Result<()> {
        if frequency == 0 {
            anyhow::bail!("frequency must be at least 1");
        }
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        ensure_path_exists(pl, path)?;
        pl.frequencies.insert(path.to_string(), frequency);
        Ok(())
    }

    pub fn set_shuffle(&mut self, name: &str, shuffle: bool) -> Result<()> {
        let pl = self
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        pl.shuffle = shuffle;
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

    /// Pick from a playlist, resolving relative paths against every configured
    /// source root until an existing file is found.
    pub fn pick_from_roots(
        &self,
        source_roots: &[&Path],
        cursor: &mut HashMap<String, usize>,
        recent_window: usize,
        recent_paths: &VecDeque<PathBuf>,
        excluded_paths: &HashSet<PathBuf>,
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
                if !resolved.is_file() || excluded_paths.contains(&resolved) {
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
    crate::config::default_config_path().with_file_name("playlists.kdl")
}

// ---------------------------------------------------------------------------
// KDL parser
// ---------------------------------------------------------------------------

/// Parse a `playlists.kdl` file into a `PlaylistStore`.
/// Uses the same BOM-tolerant, hand-rolled approach as `config/parse.rs`.
pub fn parse_playlists(input: &str) -> Result<PlaylistStore> {
    let input = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let mut store = PlaylistStore::default();

    let mut in_playlist: Option<Playlist> = None;

    for (line_index, raw) in input.lines().enumerate() {
        let line_number = line_index + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if line == "}" {
            let mut playlist = in_playlist.take().ok_or_else(|| {
                anyhow::anyhow!("playlists line {line_number}: unexpected closing brace")
            })?;
            normalize_playlist(&mut playlist);
            validate_playlist(&playlist)
                .map_err(|error| anyhow::anyhow!("playlists line {line_number}: {error:#}"))?;
            if store.get(&playlist.name).is_some() {
                anyhow::bail!(
                    "playlists line {line_number}: duplicate playlist '{}'",
                    playlist.name
                );
            }
            store.playlists.push(playlist);
            continue;
        }

        if let Some(rest) = node_args(line, "playlist") {
            if let Some(open) = &in_playlist {
                anyhow::bail!(
                    "playlists line {line_number}: playlist '{}' block is still open",
                    open.name
                );
            }
            let body = rest.strip_suffix('{').ok_or_else(|| {
                anyhow::anyhow!("playlists line {line_number}: playlist must end with '{{'")
            })?;
            let values = parse_quoted_values(body)
                .map_err(|error| anyhow::anyhow!("playlists line {line_number}: {error:#}"))?;
            let [name] = values.as_slice() else {
                anyhow::bail!("playlists line {line_number}: playlist expects one quoted name");
            };
            validate_playlist_name(name)
                .map_err(|error| anyhow::anyhow!("playlists line {line_number}: {error}"))?;
            in_playlist = Some(Playlist {
                name: name.clone(),
                ..Playlist::default()
            });
            continue;
        }

        if in_playlist.is_none() {
            if let Some(rest) = node_args(line, "active") {
                if store.active.is_some() {
                    anyhow::bail!("playlists line {line_number}: duplicate active directive");
                }
                let values = parse_quoted_values(rest)
                    .map_err(|error| anyhow::anyhow!("playlists line {line_number}: {error:#}"))?;
                let [name] = values.as_slice() else {
                    anyhow::bail!(
                        "playlists line {line_number}: active expects one quoted playlist name"
                    );
                };
                store.active = Some(name.clone());
                continue;
            }
            let (key, _) = split_node(line);
            anyhow::bail!("playlists line {line_number}: unknown top-level node '{key}'");
        }

        let (key, args) = split_node(line);
        let playlist = in_playlist.as_mut().unwrap();
        let result: Result<()> = (|| {
            match key {
                "shuffle" => playlist.shuffle = parse_bool(args)?,
                "path" => {
                    let values = parse_quoted_values(args)?;
                    let [path] = values.as_slice() else {
                        anyhow::bail!("path expects one quoted value");
                    };
                    validate_playlist_path(path)?;
                    playlist.paths.push(path.clone());
                }
                "tag" => {
                    let values = parse_quoted_values(args)?;
                    match values.as_slice() {
                        [path, tag] => push_group_tag(playlist, path, "general", tag)?,
                        [path, kind, tag] => push_group_tag(playlist, path, kind, tag)?,
                        _ => anyhow::bail!("tag expects path/tag or path/kind/tag"),
                    }
                }
                "theme" | "content" | "color" | "source" | "medium" | "safety" | "franchise"
                | "character" => {
                    let values = parse_quoted_values(args)?;
                    let [path, tag] = values.as_slice() else {
                        anyhow::bail!("{key} expects a quoted path and tag");
                    };
                    push_group_tag(playlist, path, key, tag)?;
                }
                "facet" => {
                    let values = parse_quoted_values(args)?;
                    let [path, kind, tag] = values.as_slice() else {
                        anyhow::bail!("facet expects a quoted path, kind, and tag");
                    };
                    push_group_tag(playlist, path, kind, tag)?;
                }
                "rating" => {
                    let (path, value) = parse_quoted_and_token(args)?;
                    let rating = value
                        .parse::<u8>()
                        .context("rating must be an integer from 0 to 5")?;
                    if rating > 5 {
                        anyhow::bail!("rating must be between 0 and 5");
                    }
                    playlist.ratings.insert(path, rating);
                }
                "frequency" => {
                    let (path, value) = parse_quoted_and_token(args)?;
                    let frequency = value
                        .parse::<u32>()
                        .context("frequency must be a positive integer")?;
                    if frequency == 0 {
                        anyhow::bail!("frequency must be at least 1");
                    }
                    playlist.frequencies.insert(path, frequency);
                }
                _ => anyhow::bail!("unknown playlist node '{key}'"),
            }
            Ok(())
        })();
        result.map_err(|error| anyhow::anyhow!("playlists line {line_number}: {error:#}"))?;
    }

    if let Some(playlist) = in_playlist {
        anyhow::bail!("unclosed playlist '{}' block", playlist.name);
    }
    validate_store(&store)?;
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
        let mut seen = HashSet::new();
        let paths: Vec<&String> = pl
            .paths
            .iter()
            .filter(|path| seen.insert(path.as_str()))
            .collect();
        for path in &paths {
            out.push_str(&format!("    path \"{}\"\n", escape_kdl(path)));
        }
        for path in paths {
            for (kind, group) in &pl.tag_groups {
                let Some(tags) = group.get(path) else {
                    continue;
                };
                match kind.as_str() {
                    "general" => write_tag_lines(&mut out, "tag", path, Some(tags)),
                    "theme" | "content" | "color" | "source" | "medium" | "safety"
                    | "franchise" | "character" => {
                        write_tag_lines(&mut out, kind, path, Some(tags));
                    }
                    _ => {
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
    validate_store(store).context("validate playlists before persist")?;
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

pub(crate) fn replace_file(tmp: &Path, path: &Path) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    unsafe {
        MoveFileExW(
            &HSTRING::from(tmp),
            &HSTRING::from(path),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
    }
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

fn split_node(line: &str) -> (&str, &str) {
    match line.find(char::is_whitespace) {
        Some(index) => (&line[..index], line[index..].trim()),
        None => (line, ""),
    }
}

fn node_args<'a>(line: &'a str, expected: &str) -> Option<&'a str> {
    let (node, args) = split_node(line);
    (node == expected).then_some(args)
}

fn parse_quoted_prefix(input: &str) -> Result<(String, &str)> {
    let input = input.trim_start();
    let bytes = input.as_bytes();
    let quote = match bytes.first() {
        Some(b'"' | b'\'') => bytes[0],
        _ => anyhow::bail!("expected a quoted string"),
    };
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == quote {
            let tail = &input[index + 1..];
            if tail
                .chars()
                .next()
                .is_some_and(|character| !character.is_whitespace())
            {
                anyhow::bail!("quoted values must be separated by whitespace");
            }
            return Ok((unescape_kdl(&input[1..index])?, tail));
        }
    }
    anyhow::bail!("unterminated quoted string")
}

fn parse_quoted_values(mut input: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    while !input.trim().is_empty() {
        let (value, tail) = parse_quoted_prefix(input)?;
        values.push(value);
        input = tail;
    }
    Ok(values)
}

fn parse_quoted_and_token(input: &str) -> Result<(String, &str)> {
    let (value, tail) = parse_quoted_prefix(input)?;
    let mut tokens = tail.split_whitespace();
    let token = tokens
        .next()
        .ok_or_else(|| anyhow::anyhow!("expected a value after the quoted path"))?;
    if tokens.next().is_some() {
        anyhow::bail!("unexpected trailing values");
    }
    Ok((value, token))
}

fn validate_playlist(playlist: &Playlist) -> Result<()> {
    validate_playlist_name(&playlist.name)?;
    for path in &playlist.paths {
        validate_playlist_path(path)?;
    }
    for (kind, map) in &playlist.tag_groups {
        validate_tag_kind(kind)?;
        for path in map.keys() {
            ensure_path_exists(playlist, path)?;
        }
    }
    for (path, rating) in &playlist.ratings {
        ensure_path_exists(playlist, path)?;
        if *rating > 5 {
            anyhow::bail!("rating must be between 0 and 5");
        }
    }
    for (path, frequency) in &playlist.frequencies {
        ensure_path_exists(playlist, path)?;
        if *frequency == 0 {
            anyhow::bail!("frequency must be at least 1");
        }
    }
    Ok(())
}

fn validate_store(store: &PlaylistStore) -> Result<()> {
    let mut names = HashSet::new();
    for playlist in &store.playlists {
        validate_playlist(playlist)?;
        if !names.insert(playlist.name.as_str()) {
            anyhow::bail!("duplicate playlist '{}'", playlist.name);
        }
    }
    if let Some(active) = &store.active {
        validate_playlist_name(active)?;
        if store.get(active).is_none() {
            anyhow::bail!("active playlist '{}' does not exist", active);
        }
    }
    Ok(())
}

fn validate_playlist_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("playlist name must not be blank");
    }
    Ok(())
}

fn validate_playlist_path(path: &str) -> Result<()> {
    if path.trim().is_empty() {
        anyhow::bail!("playlist path must not be blank");
    }
    Ok(())
}

fn validate_tag_kind(kind: &str) -> Result<()> {
    if kind.trim().is_empty() {
        anyhow::bail!("tag kind must not be blank");
    }
    Ok(())
}

fn normalize_playlist(playlist: &mut Playlist) {
    dedupe_strings(&mut playlist.paths);
    for map in playlist.tag_groups.values_mut() {
        for tags in map.values_mut() {
            dedupe_strings(tags);
        }
    }
}

fn dedupe_strings(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn ensure_path_exists(pl: &Playlist, path: &str) -> Result<()> {
    validate_playlist_path(path)?;
    if pl.paths.iter().any(|p| p == path) {
        Ok(())
    } else {
        anyhow::bail!("path '{}' not in playlist '{}'", path, pl.name)
    }
}

fn clear_playlist_path_metadata(pl: &mut Playlist, path: &str) {
    pl.tag_groups.retain(|_, group| {
        group.remove(path);
        !group.is_empty()
    });
    pl.ratings.remove(path);
    pl.frequencies.remove(path);
}

fn normalized_tag_kind(kind: &str) -> Option<&'static str> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "general" | "tag" | "tags" => Some("general"),
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

fn canonical_tag_kind(kind: &str) -> Result<String> {
    validate_tag_kind(kind)?;
    if let Some(kind) = normalized_tag_kind(kind) {
        return Ok(kind.to_string());
    }
    let kind = normalize_custom_kind(kind);
    if kind.is_empty() {
        anyhow::bail!("empty tag kind");
    }
    Ok(kind)
}

fn set_group_map(pl: &mut Playlist, path: &str, kind: &str, tags: Vec<String>) -> Result<()> {
    let kind = canonical_tag_kind(kind)?;
    if tags.is_empty() {
        if pl.tag_groups.get_mut(&kind).is_some_and(|map| {
            map.remove(path);
            map.is_empty()
        }) {
            pl.tag_groups.remove(&kind);
        }
    } else {
        pl.tag_groups
            .entry(kind)
            .or_default()
            .insert(path.to_string(), tags);
    }
    Ok(())
}

fn push_group_tag(pl: &mut Playlist, path: &str, kind: &str, tag: &str) -> Result<()> {
    let kind = canonical_tag_kind(kind)?;
    let tag = tag.trim();
    if tag.is_empty() {
        return Ok(());
    }
    pl.tag_groups
        .entry(kind)
        .or_default()
        .entry(path.to_string())
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

fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => anyhow::bail!("boolean must be true or false"),
    }
}

/// Escape a string for inclusion inside KDL double-quotes.
fn escape_kdl(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for character in s.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if control.is_control() => {
                out.push_str(&format!("\\u{{{:x}}}", control as u32));
            }
            other => out.push(other),
        }
    }
    out
}

fn unescape_kdl(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('u') => {
                    if chars.next() != Some('{') {
                        anyhow::bail!("unicode escape must use \\u{{hex}} syntax");
                    }
                    let mut hex = String::new();
                    loop {
                        match chars.next() {
                            Some('}') if !hex.is_empty() => break,
                            Some(digit) if digit.is_ascii_hexdigit() && hex.len() < 6 => {
                                hex.push(digit);
                            }
                            _ => anyhow::bail!("invalid unicode escape"),
                        }
                    }
                    let value = u32::from_str_radix(&hex, 16).context("invalid unicode escape")?;
                    out.push(
                        char::from_u32(value)
                            .ok_or_else(|| anyhow::anyhow!("invalid Unicode scalar value"))?,
                    );
                }
                Some(other) => anyhow::bail!("unsupported escape \\{other}"),
                None => anyhow::bail!("trailing backslash in quoted string"),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
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
            pl.tag_groups["general"]
                .get(r#"folder//wall\"paper.jpg"#)
                .unwrap(),
            &vec!["calm//blue".to_string()]
        );
    }

    #[test]
    fn duplicate_paths_and_metadata_converge_after_parse_and_persist() {
        let source = r#"
playlist "legacy" {
    path "same.jpg"
    path "same.jpg"
    tag "same.jpg" " calm "
    tag "same.jpg" "calm"
}
"#;
        let parsed = parse_playlists(source).unwrap();
        let playlist = parsed.get("legacy").unwrap();
        assert_eq!(playlist.paths, ["same.jpg"]);
        assert_eq!(playlist.tag_groups["general"]["same.jpg"], ["calm"]);

        let serialized = serialize_playlists(&parsed);
        assert_eq!(
            serialized
                .lines()
                .filter(|line| line.trim() == "path \"same.jpg\"")
                .count(),
            1
        );
        assert_eq!(
            serialized
                .lines()
                .filter(|line| line.trim() == "tag \"same.jpg\" \"calm\"")
                .count(),
            1
        );
        let reparsed = parse_playlists(&serialized).unwrap();
        assert_eq!(reparsed.get("legacy").unwrap().paths, ["same.jpg"]);
        assert_eq!(
            reparsed.get("legacy").unwrap().tag_groups["general"]["same.jpg"],
            ["calm"]
        );
    }

    #[test]
    fn strict_parser_rejects_incomplete_or_invalid_files() {
        let cases = [
            (
                "playlist \"open\" {\n    path \"a.jpg\"\n",
                "unclosed playlist 'open' block",
            ),
            (
                "playlist \"bad\" {\n    mystery true\n}\n",
                "unknown playlist node 'mystery'",
            ),
            (
                "playlist \"bad\" {\n    shuffle perhaps\n}\n",
                "boolean must be true or false",
            ),
            (
                "active \"missing\"\n",
                "active playlist 'missing' does not exist",
            ),
            (
                "playlist \"bad\" {\n    rating \"missing.jpg\" 4\n}\n",
                "path 'missing.jpg' not in playlist 'bad'",
            ),
            ("playlist \"   \" {\n}\n", "playlist name must not be blank"),
            (
                "playlist \"bad\" {\n    path \"   \"\n}\n",
                "playlist path must not be blank",
            ),
            (
                "playlist \"bad\" {\n    path \"a.jpg\"\n    rating \"a.jpg\" 6\n}\n",
                "rating must be between 0 and 5",
            ),
            (
                "playlist \"bad\" {\n    path \"a.jpg\"\n    frequency \"a.jpg\" 0\n}\n",
                "frequency must be at least 1",
            ),
        ];

        for (source, expected) in cases {
            let error = parse_playlists(source).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "{error:?} did not contain {expected:?}"
            );
        }
    }

    #[test]
    fn serialization_escapes_control_characters() {
        let name = "odd\nname";
        let path = "C:\\wallpapers\\odd\t\"name\"\u{7}.jpg";
        let mut store = PlaylistStore::default();
        store.create(name).unwrap();
        store.add_path(name, path).unwrap();
        store
            .set_tags(name, path, vec!["line\rbreak".to_string()])
            .unwrap();
        store.activate(name).unwrap();

        let serialized = serialize_playlists(&store);
        assert!(!serialized.contains('\u{7}'));
        let reparsed = parse_playlists(&serialized).unwrap();
        assert_eq!(reparsed.active.as_deref(), Some(name));
        assert_eq!(reparsed.get(name).unwrap().paths, vec![path]);
        assert_eq!(
            reparsed.get(name).unwrap().tag_groups["general"]
                .get(path)
                .unwrap(),
            &vec!["line\rbreak".to_string()]
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
        store.set_shuffle("alpha", true).unwrap();
        assert!(store.get("alpha").unwrap().shuffle);
        assert!(store.set_shuffle("missing", true).is_err());
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
    fn store_rejects_invalid_values_and_deduplicates_trimmed_tags() {
        let mut store = PlaylistStore::default();
        assert!(store.create(" \t").is_err());
        store.create("valid").unwrap();
        assert!(store.add_path("valid", " \t").is_err());
        store.add_path("valid", "photo.jpg").unwrap();
        assert!(store.set_rating("valid", "photo.jpg", 6).is_err());
        assert!(store.set_frequency("valid", "photo.jpg", 0).is_err());
        store
            .set_tags(
                "valid",
                "photo.jpg",
                vec![" calm ".into(), "calm".into(), " blue ".into()],
            )
            .unwrap();

        let playlist = store.get("valid").unwrap();
        assert!(!playlist.ratings.contains_key("photo.jpg"));
        assert!(!playlist.frequencies.contains_key("photo.jpg"));
        assert_eq!(
            playlist.tag_groups["general"]["photo.jpg"],
            ["calm", "blue"]
        );
    }

    #[test]
    fn set_tag_group_rejects_blank_kind_without_mutating_general_tags() {
        let mut store = PlaylistStore::default();
        store.create("valid").unwrap();
        store.add_path("valid", "photo.jpg").unwrap();

        let error = store
            .set_tag_group("valid", "photo.jpg", " \t", vec!["calm".into()])
            .unwrap_err()
            .to_string();

        assert!(error.contains("tag kind must not be blank"));
        let playlist = store.get("valid").unwrap();
        assert!(playlist.tag_groups.is_empty());
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
        let excluded = HashSet::new();
        let p1 = store
            .pick_from_roots(&[], &mut cursor, 0, &recent, &excluded)
            .unwrap();
        let p2 = store
            .pick_from_roots(&[], &mut cursor, 0, &recent, &excluded)
            .unwrap();
        let p3 = store
            .pick_from_roots(&[], &mut cursor, 0, &recent, &excluded)
            .unwrap(); // wraps back to first
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
            .pick_from_roots(&roots, &mut cursor, 0, &recent, &HashSet::new())
            .unwrap();
        assert_eq!(first, root_a.path().join("first.jpg"));

        recent.push_back(first);
        let second = store
            .pick_from_roots(&roots, &mut cursor, 1, &recent, &HashSet::new())
            .unwrap();
        assert_eq!(second, root_b.path().join("second.jpg"));
    }

    #[test]
    fn active_playlist_skips_banned_paths() {
        let root = tempfile::tempdir().unwrap();
        let banned = root.path().join("banned.jpg");
        let allowed = root.path().join("allowed.jpg");
        std::fs::write(&banned, b"banned").unwrap();
        std::fs::write(&allowed, b"allowed").unwrap();

        let mut store = PlaylistStore::default();
        store.create("filtered").unwrap();
        store.add_path("filtered", "banned.jpg").unwrap();
        store.add_path("filtered", "allowed.jpg").unwrap();
        store.activate("filtered").unwrap();

        assert_eq!(
            store.pick_from_roots(
                &[root.path()],
                &mut HashMap::new(),
                0,
                &VecDeque::new(),
                &HashSet::from([banned]),
            ),
            Some(allowed)
        );
    }

    #[test]
    fn active_playlist_returns_none_when_every_path_is_banned() {
        let root = tempfile::tempdir().unwrap();
        let banned = root.path().join("banned.jpg");
        std::fs::write(&banned, b"banned").unwrap();

        let mut store = PlaylistStore::default();
        store.create("filtered").unwrap();
        store.add_path("filtered", "banned.jpg").unwrap();
        store.activate("filtered").unwrap();

        assert_eq!(
            store.pick_from_roots(
                &[root.path()],
                &mut HashMap::new(),
                0,
                &VecDeque::new(),
                &HashSet::from([banned]),
            ),
            None
        );
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
            pl.tag_groups["general"].get("a.jpg").unwrap(),
            &vec!["calm".to_string(), "blue".to_string()]
        );
        assert_eq!(
            pl.tag_groups["theme"].get("a.jpg").unwrap(),
            &vec!["minimal".to_string()]
        );
        assert_eq!(
            pl.tag_groups["content"].get("a.jpg").unwrap(),
            &vec!["mountain".to_string()]
        );
        assert_eq!(
            pl.tag_groups["color"].get("a.jpg").unwrap(),
            &vec!["cyan".to_string()]
        );
        assert_eq!(
            pl.tag_groups["source"].get("a.jpg").unwrap(),
            &vec!["wallhaven".to_string()]
        );
        assert_eq!(
            pl.tag_groups["medium"].get("a.jpg").unwrap(),
            &vec!["anime".to_string()]
        );
        assert_eq!(
            pl.tag_groups["safety"].get("a.jpg").unwrap(),
            &vec!["nsfw".to_string()]
        );
        assert_eq!(
            pl.tag_groups["franchise"].get("a.jpg").unwrap(),
            &vec!["naruto".to_string()]
        );
        assert_eq!(
            pl.tag_groups["character"].get("a.jpg").unwrap(),
            &vec!["naruto-uzumaki".to_string()]
        );
        assert_eq!(
            pl.tag_groups["artist"].get("a.jpg").unwrap(),
            &vec!["kishimoto".to_string()]
        );
        assert_eq!(pl.ratings.get("a.jpg"), Some(&4));
        assert_eq!(pl.frequencies.get("a.jpg"), Some(&3));

        let serialized = serialize_playlists(&store);
        let reparsed = parse_playlists(&serialized).expect("reparse");
        let pl = reparsed.get("focus").expect("playlist");
        assert_eq!(pl.tag_groups["general"].get("a.jpg").unwrap().len(), 2);
        assert_eq!(
            pl.tag_groups["theme"].get("a.jpg").unwrap(),
            &vec!["minimal".to_string()]
        );
        assert_eq!(
            pl.tag_groups["content"].get("a.jpg").unwrap(),
            &vec!["mountain".to_string()]
        );
        assert_eq!(
            pl.tag_groups["color"].get("a.jpg").unwrap(),
            &vec!["cyan".to_string()]
        );
        assert_eq!(
            pl.tag_groups["source"].get("a.jpg").unwrap(),
            &vec!["wallhaven".to_string()]
        );
        assert_eq!(
            pl.tag_groups["medium"].get("a.jpg").unwrap(),
            &vec!["anime".to_string()]
        );
        assert_eq!(
            pl.tag_groups["safety"].get("a.jpg").unwrap(),
            &vec!["nsfw".to_string()]
        );
        assert_eq!(
            pl.tag_groups["franchise"].get("a.jpg").unwrap(),
            &vec!["naruto".to_string()]
        );
        assert_eq!(
            pl.tag_groups["character"].get("a.jpg").unwrap(),
            &vec!["naruto-uzumaki".to_string()]
        );
        assert_eq!(
            pl.tag_groups["artist"].get("a.jpg").unwrap(),
            &vec!["kishimoto".to_string()]
        );
        assert_eq!(pl.ratings.get("a.jpg"), Some(&4));
        assert_eq!(pl.frequencies.get("a.jpg"), Some(&3));
    }

    #[test]
    fn test_atomic_persist_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("playlists.kdl");

        let invalid = PlaylistStore {
            playlists: vec![Playlist {
                name: " ".to_string(),
                ..Playlist::default()
            }],
            active: None,
        };
        assert!(persist_playlists(&invalid, &path).is_err());
        assert!(!path.exists());

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

    #[test]
    fn persist_rejects_duplicate_playlist_names() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("duplicates.kdl");
        let playlist = Playlist {
            name: "same".to_string(),
            ..Playlist::default()
        };
        let store = PlaylistStore {
            playlists: vec![playlist.clone(), playlist],
            active: None,
        };

        let error = format!("{:#}", persist_playlists(&store, &path).unwrap_err());

        assert!(error.contains("duplicate playlist 'same'"));
        assert!(!path.exists());
    }

    #[test]
    fn persist_rejects_blank_custom_tag_kinds() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blank-custom-kind.kdl");
        let mut playlist = Playlist {
            name: "valid".to_string(),
            paths: vec!["photo.jpg".to_string()],
            ..Playlist::default()
        };
        playlist.tag_groups.insert(
            " \t".to_string(),
            HashMap::from([("photo.jpg".to_string(), vec!["calm".to_string()])]),
        );
        let store = PlaylistStore {
            playlists: vec![playlist],
            active: None,
        };

        let error = format!("{:#}", persist_playlists(&store, &path).unwrap_err());

        assert!(error.contains("tag kind must not be blank"));
        assert!(!path.exists());
    }
}

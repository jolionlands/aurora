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
use std::collections::HashMap;
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
            Ok(())
        } else {
            anyhow::bail!("path '{}' not in playlist '{}'", path, name);
        }
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
        // Sequential cursor: maps playlist_name → next_index.  Updated in-place.
        cursor: &mut HashMap<String, usize>,
    ) -> Option<PathBuf> {
        let pl = self.active_playlist()?;

        // Resolve all existing paths.
        let existing: Vec<PathBuf> = pl
            .paths
            .iter()
            .map(|s| {
                let p = PathBuf::from(s);
                if p.is_absolute() {
                    p
                } else if let Some(root) = source_root {
                    root.join(&p)
                } else {
                    p
                }
            })
            .filter(|p| p.exists())
            .collect();

        if existing.is_empty() {
            return None;
        }

        if pl.shuffle {
            let idx = rand::thread_rng().gen_range(0..existing.len());
            Some(existing[idx].clone())
        } else {
            // Sequential: advance cursor.
            let cur = cursor.entry(pl.name.clone()).or_insert(0);
            // Wrap around.
            if *cur >= existing.len() {
                *cur = 0;
            }
            let path = existing[*cur].clone();
            *cur += 1;
            Some(path)
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
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
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
    if let Some(pos) = s.find("//") {
        s[..pos].trim_end()
    } else {
        s
    }
}

/// Extract the first double- or single-quoted string from `s`.
fn extract_quoted(s: &str) -> Option<String> {
    let start = s.find(['"', '\''])?;
    let quote = s.as_bytes()[start];
    let inner = &s[start + 1..];
    let end = inner.find(quote as char)?;
    Some(unescape_kdl(&inner[..end]))
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

        store
            .add_path("seq", &a.to_string_lossy())
            .unwrap();
        store
            .add_path("seq", &b.to_string_lossy())
            .unwrap();
        store.activate("seq").unwrap();

        let mut cursor = HashMap::new();
        let p1 = store.pick(None, &mut cursor).unwrap();
        let p2 = store.pick(None, &mut cursor).unwrap();
        let p3 = store.pick(None, &mut cursor).unwrap(); // wraps back to first
        assert_ne!(p1, p2);
        assert_eq!(p1, p3);
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

        let loaded = load_playlists(&path).unwrap();
        assert_eq!(loaded.playlists.len(), 1);
        assert_eq!(loaded.active, Some("x".to_string()));
    }
}

//! Resolving playlist entries, paths, and content IDs to indexed images.

use super::*;

pub(super) fn indexed_hash(index: &PhotoIndex, path: &Path) -> Option<String> {
    indexed_entry_for_path(index, path).map(|entry| entry.hash.clone())
}

/// `\\?\C:\x` -> `C:\x` and `\\?\UNC\s\x` -> `\\s\x`, so a canonicalized path
/// compares equal to the plain path an index scan recorded.
pub(super) fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

pub(super) fn target_hash(
    index: &RwLock<PhotoIndex>,
    path: &Path,
    known_hash: Option<String>,
) -> Result<String> {
    if let Some(hash) = known_hash.or_else(|| indexed_hash(&index.read(), path)) {
        Ok(hash)
    } else {
        crate::index::hash_file(path)
            .with_context(|| format!("hash selected wallpaper {}", path.display()))
    }
}

pub(super) fn banned_paths(index: &PhotoIndex) -> HashSet<PathBuf> {
    index
        .photos
        .iter()
        .filter(|entry| entry.banned)
        .map(|entry| entry.path.clone())
        .collect()
}

pub(super) fn dynamic_playlist_entries<'a>(
    index: &'a PhotoIndex,
    content: &'a ContentStore,
    filters: Option<&'a TagFilters>,
) -> impl Iterator<Item = &'a PhotoEntry> {
    index.photos.iter().filter(move |entry| {
        !entry.banned
            && filters.is_none_or(|filters| {
                filters.accepts(
                    content
                        .get(&entry.hash)
                        .map(|metadata| &metadata.tag_groups),
                )
            })
    })
}

pub(super) fn dynamic_playlist_candidates(
    index: &PhotoIndex,
    content: &ContentStore,
    filters: Option<&TagFilters>,
    excluded_paths: &HashSet<PathBuf>,
) -> Vec<(PathBuf, u64)> {
    dynamic_playlist_entries(index, content, filters)
        .filter(|entry| !excluded_paths.contains(&entry.path))
        .map(|entry| {
            let rating_weight = content
                .get(&entry.hash)
                .and_then(|metadata| metadata.rating)
                .map(|rating| u64::from(rating) + 1)
                .unwrap_or(1);
            (entry.path.clone(), rating_weight)
        })
        .collect()
}

pub(super) fn rotation_target(
    index: &PhotoIndex,
    playlist_active: bool,
    playlist_pick: Option<(PathBuf, String)>,
    recent_window: usize,
    recent_paths: &VecDeque<PathBuf>,
) -> Result<(PathBuf, Option<String>)> {
    if let Some((path, hash)) = playlist_pick {
        return Ok((path, Some(hash)));
    }
    if playlist_active {
        anyhow::bail!(
            "active playlist has no eligible accessible non-banned files; run `aurora-ctl playlist deactivate` to resume full-index rotation"
        );
    }
    let photo = index
        .pick_random(recent_window, recent_paths)
        .ok_or_else(|| anyhow::anyhow!("photo index is empty or all photos are banned"))?;
    Ok((photo.path.clone(), Some(photo.hash.clone())))
}

pub(super) fn resolved_playlist_path(stored: &str, source_roots: &[PathBuf]) -> Option<PathBuf> {
    let path = PathBuf::from(stored);
    if path.is_absolute() || source_roots.is_empty() {
        return path.is_file().then_some(path);
    }
    source_roots
        .iter()
        .map(|root| root.join(&path))
        .find(|candidate| candidate.is_file())
}

pub(super) fn playlist_path_tag_groups(
    playlist: &Playlist,
    path: &str,
) -> BTreeMap<String, Vec<String>> {
    playlist
        .tag_groups
        .iter()
        .filter_map(|(kind, paths)| paths.get(path).cloned().map(|tags| (kind.clone(), tags)))
        .collect()
}

pub(super) fn effective_tag_groups(
    playlist: &Playlist,
    path: &str,
    metadata: Option<&ContentMetadata>,
    include_legacy: bool,
) -> BTreeMap<String, Vec<String>> {
    let mut groups = if metadata.is_none() || include_legacy {
        playlist_path_tag_groups(playlist, path)
    } else {
        BTreeMap::new()
    };
    if let Some(metadata) = metadata {
        for (kind, tags) in &metadata.tag_groups {
            groups.insert(kind.clone(), tags.clone());
        }
    }
    groups
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedContent {
    pub(super) hash: String,
    pub(super) path: PathBuf,
    pub(super) aliases: Vec<String>,
    pub(super) width: Option<u32>,
    pub(super) height: Option<u32>,
}

/// Find the index entry for `path` without touching the disk in the common
/// case. Callers often pass canonical (`\\?\`) paths while the index keeps
/// the scanned spelling, so both forms are compared first. Only a real
/// spelling difference (case, `..`, links) falls back to canonicalizing, and
/// then only for entries with the same file name.
pub(super) fn indexed_entry_for_path<'a>(
    index: &'a PhotoIndex,
    path: &Path,
) -> Option<&'a PhotoEntry> {
    let plain = strip_verbatim_prefix(path);
    if let Some(entry) = index
        .photos
        .iter()
        .find(|entry| entry.path == path || entry.path == plain)
    {
        return Some(entry);
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    let name = canonical.file_name()?.to_string_lossy().to_lowercase();
    index.photos.iter().find(|entry| {
        entry
            .path
            .file_name()
            .is_some_and(|entry_name| entry_name.to_string_lossy().to_lowercase() == name)
            && std::fs::canonicalize(&entry.path).is_ok_and(|entry_path| entry_path == canonical)
    })
}

pub(super) fn resolve_content(
    index: &PhotoIndex,
    content: &ContentStore,
    stored: &str,
    source_roots: &[PathBuf],
    hash_unindexed: bool,
) -> Result<Option<ResolvedContent>> {
    if let Some(path) = resolved_playlist_path(stored, source_roots) {
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if let Some(entry) = indexed_entry_for_path(index, &canonical) {
            return Ok(Some(ResolvedContent {
                hash: entry.hash.clone(),
                path: canonical.clone(),
                aliases: vec![stored.to_string(), canonical.to_string_lossy().into_owned()],
                width: entry.width,
                height: entry.height,
            }));
        }
        if hash_unindexed {
            let (width, height) = crate::decode::validate_image_file(&canonical)
                .with_context(|| format!("identify playlist content {}", canonical.display()))?;
            return Ok(Some(ResolvedContent {
                hash: crate::index::hash_file(&canonical)?,
                path: canonical.clone(),
                aliases: vec![stored.to_string(), canonical.to_string_lossy().into_owned()],
                width: Some(width),
                height: Some(height),
            }));
        }
        let hash = match content.hash_for_alias(stored)? {
            Some(hash) => Some(hash),
            None => content.hash_for_alias(&canonical.to_string_lossy())?,
        };
        if let Some(hash) = hash {
            let metadata = content
                .get(hash)
                .expect("an alias lookup returns an existing content entry");
            return Ok(Some(ResolvedContent {
                hash: hash.to_string(),
                path: canonical.clone(),
                aliases: vec![stored.to_string(), canonical.to_string_lossy().into_owned()],
                width: metadata.width,
                height: metadata.height,
            }));
        }
        return Ok(None);
    }

    let Some(hash) = content.hash_for_alias(stored)? else {
        return Ok(None);
    };
    let Some(entry) = index.photos.iter().find(|entry| entry.hash == hash) else {
        return Ok(None);
    };
    let path = std::fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone());
    Ok(Some(ResolvedContent {
        hash: hash.to_string(),
        path: path.clone(),
        aliases: vec![stored.to_string(), path.to_string_lossy().into_owned()],
        width: entry.width,
        height: entry.height,
    }))
}

pub(super) fn content_id_hash(target: &str) -> Result<Option<String>> {
    let Some((scheme, hash)) = target.split_once(':') else {
        return Ok(None);
    };
    if !scheme.eq_ignore_ascii_case("blake3") {
        return Ok(None);
    }
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("content ID must be blake3 followed by 64 hexadecimal characters");
    }
    Ok(Some(hash.to_ascii_lowercase()))
}

pub(super) fn resolved_content_by_hash(
    index: &PhotoIndex,
    content: &ContentStore,
    hash: &str,
) -> Result<ResolvedContent> {
    let metadata = content.get(hash);
    let indexed = index.photos.iter().find(|entry| entry.hash == hash);
    if metadata.is_none() && indexed.is_none() {
        anyhow::bail!("content 'blake3:{hash}' not found");
    }

    let mut aliases = metadata
        .map(|metadata| metadata.aliases.clone())
        .unwrap_or_default();
    if let Some(entry) = indexed {
        let alias = entry.path.display().to_string();
        if !aliases.contains(&alias) {
            aliases.push(alias);
        }
    }
    let path = indexed
        .map(|entry| entry.path.clone())
        .or_else(|| {
            aliases
                .iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
        })
        .or_else(|| aliases.first().map(PathBuf::from))
        .unwrap_or_default();
    let width = metadata
        .and_then(|metadata| metadata.width)
        .or_else(|| indexed.and_then(|entry| entry.width));
    let height = metadata
        .and_then(|metadata| metadata.height)
        .or_else(|| indexed.and_then(|entry| entry.height));

    Ok(ResolvedContent {
        hash: hash.to_string(),
        path,
        aliases,
        width,
        height,
    })
}

pub(super) fn resolve_content_target(
    index: &PhotoIndex,
    content: &ContentStore,
    target: &str,
    source_roots: &[PathBuf],
) -> Result<ResolvedContent> {
    let target = target.trim();
    if target.is_empty() {
        anyhow::bail!("content target must not be empty");
    }
    if let Some(hash) = content_id_hash(target)? {
        return resolved_content_by_hash(index, content, &hash);
    }
    if resolved_playlist_path(target, source_roots).is_some() {
        return resolve_content(index, content, target, source_roots, true)?
            .ok_or_else(|| anyhow::anyhow!("cannot identify image content for path '{}'", target));
    }
    if let Some(hash) = content.hash_for_alias(target)? {
        return resolved_content_by_hash(index, content, hash);
    }
    anyhow::bail!("content target '{}' not found", target)
}

pub(super) fn resolve_playlist_entry_path(
    store: &PlaylistStore,
    name: &str,
    path: &str,
    source_roots: &[PathBuf],
) -> anyhow::Result<String> {
    let Some(playlist) = store.get(name) else {
        return Ok(path.to_string());
    };
    if playlist.paths.iter().any(|stored| stored == path) {
        return Ok(path.to_string());
    }
    let incoming = Path::new(path);
    if !incoming.is_absolute() {
        return Ok(path.to_string());
    }
    let canonical_incoming = std::fs::canonicalize(incoming).ok();
    let lexical_incoming = std::path::absolute(incoming).ok();

    let mut found: Option<&str> = None;
    for stored in &playlist.paths {
        let stored_path = Path::new(stored);
        let equivalent = if stored_path.is_absolute() || source_roots.is_empty() {
            canonical_incoming.as_ref().is_some_and(|incoming| {
                std::fs::canonicalize(stored_path).is_ok_and(|path| path == *incoming)
            })
        } else {
            let candidate = source_roots
                .iter()
                .map(|root| root.join(stored_path))
                .find(|candidate| candidate.is_file())
                .unwrap_or_else(|| source_roots[0].join(stored_path));
            canonical_incoming.as_ref().is_some_and(|incoming| {
                std::fs::canonicalize(&candidate).is_ok_and(|path| path == *incoming)
            }) || lexical_incoming.as_ref().is_some_and(|incoming| {
                std::path::absolute(&candidate).is_ok_and(|path| path == *incoming)
            })
        };
        if !equivalent {
            continue;
        }
        if let Some(first) = found {
            anyhow::bail!(
                "playlist '{name}' has multiple entries for {}: {:?} and {:?}",
                incoming.display(),
                first,
                stored
            );
        }
        found = Some(stored);
    }

    Ok(found.unwrap_or(path).to_string())
}

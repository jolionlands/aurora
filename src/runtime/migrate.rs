//! One-time import of legacy per-playlist metadata into the content store.

use super::*;

pub(super) fn migrate_legacy_content(
    content: &mut ContentStore,
    playlists: &PlaylistStore,
    index: &PhotoIndex,
    source_roots: &[PathBuf],
) -> Result<bool> {
    let indexed: HashMap<PathBuf, &PhotoEntry> = index
        .photos
        .iter()
        .filter_map(|entry| {
            std::fs::canonicalize(&entry.path)
                .ok()
                .map(|path| (path, entry))
        })
        .collect();
    let mut changed = false;

    let first_pass = content.needs_legacy_migration();
    let reconciliation = content.needs_legacy_reconciliation();
    let candidates: Vec<(String, String)> = if first_pass || reconciliation {
        playlists
            .playlists
            .iter()
            .flat_map(|playlist| {
                playlist
                    .paths
                    .iter()
                    .map(|path| (playlist.name.clone(), path.clone()))
            })
            .collect()
    } else {
        content
            .pending_legacy()
            .map(|(playlist, path)| (playlist.to_string(), path.to_string()))
            .collect()
    };
    let mut pending = BTreeSet::new();

    for (playlist_name, stored) in candidates {
        let Some(playlist) = playlists.get(&playlist_name) else {
            continue;
        };
        if !playlist.paths.iter().any(|path| path == &stored) {
            continue;
        }
        let groups = playlist_path_tag_groups(playlist, &stored);
        let rating = playlist.ratings.get(&stored).copied();
        let has_metadata = !groups.is_empty() || rating.is_some();
        let Some(resolved) = resolved_playlist_path(&stored, source_roots) else {
            if has_metadata {
                pending.insert((playlist_name, stored));
            }
            continue;
        };
        let canonical = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
        let identity = if let Some(entry) = indexed.get(&canonical) {
            (
                entry.hash.clone(),
                entry.width,
                entry.height,
                canonical.clone(),
            )
        } else {
            let (width, height) = match crate::decode::validate_image_file(&resolved) {
                Ok(dimensions) => dimensions,
                Err(error) => {
                    warn!(
                        "deferring legacy metadata migration for {}: {error:#}",
                        resolved.display()
                    );
                    if has_metadata {
                        pending.insert((playlist_name, stored));
                    }
                    continue;
                }
            };
            let hash = match crate::index::hash_file(&resolved) {
                Ok(hash) => hash,
                Err(error) => {
                    warn!(
                        "deferring legacy metadata migration for {}: {error:#}",
                        resolved.display()
                    );
                    if has_metadata {
                        pending.insert((playlist_name, stored));
                    }
                    continue;
                }
            };
            (hash, Some(width), Some(height), canonical.clone())
        };
        let aliases = vec![stored, identity.3.to_string_lossy().into_owned()];
        let already_migrated = reconciliation
            && !first_pass
            && content.get(&identity.0).is_some_and(|metadata| {
                aliases.iter().any(|alias| metadata.aliases.contains(alias))
            });
        if !already_migrated {
            changed |= content.merge_legacy(
                &identity.0,
                &aliases,
                &groups,
                rating,
                (identity.1, identity.2),
            )?;
        }
    }

    changed |= content.replace_pending_legacy(pending)?;
    if first_pass {
        changed |= content.finish_legacy_migration();
    }
    if reconciliation {
        changed |= content.finish_legacy_reconciliation();
    }

    // If a known file moved, keep the new indexed path as another alias for
    // the same exact bytes. Missing legacy paths can then resolve by hash.
    for entry in &index.photos {
        if content.get(&entry.hash).is_none() {
            continue;
        }
        let alias = std::fs::canonicalize(&entry.path)
            .unwrap_or_else(|_| entry.path.clone())
            .to_string_lossy()
            .into_owned();
        changed |= content.remember_aliases(&entry.hash, &[alias], (entry.width, entry.height))?;
    }
    Ok(changed)
}

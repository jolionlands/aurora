//! JSON views returned over IPC, bounded to the frame size.

use super::*;

pub(super) fn validate_autotag_target(name: &str, path: &str) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("playlist name must not be empty");
    }
    if path.trim().is_empty() {
        anyhow::bail!("playlist path must not be empty");
    }
    Ok(())
}

pub(super) fn playlist_summary_json(
    playlist: &Playlist,
    active: Option<&str>,
    filters: Option<&TagFilters>,
    dynamic: bool,
    path_count: usize,
) -> serde_json::Value {
    serde_json::json!({
        "name": playlist.name,
        "shuffle": playlist.shuffle,
        "path_count": path_count,
        "active": active == Some(playlist.name.as_str()),
        "dynamic": dynamic,
        "include_tags": filters.map(|filters| filters.include.clone()).unwrap_or_default(),
        "exclude_tags": filters.map(|filters| filters.exclude.clone()).unwrap_or_default(),
    })
}

pub(super) fn playlist_item_json(
    playlist: &Playlist,
    path: &str,
    identity: Option<&ResolvedContent>,
    metadata: Option<&ContentMetadata>,
    include_legacy: bool,
) -> serde_json::Value {
    let mut tag_groups = serde_json::Map::new();
    for (kind, tags) in effective_tag_groups(playlist, path, metadata, include_legacy) {
        if !tags.is_empty() {
            tag_groups.insert(kind, serde_json::json!(tags));
        }
    }
    serde_json::json!({
        "path": path,
        "resolved_path": identity.map(|identity| identity.path.display().to_string()),
        "content_id": identity.map(|identity| format!("blake3:{}", identity.hash)),
        "tag_groups": tag_groups,
        "rating": metadata
            .and_then(|metadata| metadata.rating)
            .or_else(|| playlist.ratings.get(path).copied()),
        "frequency": playlist.frequencies.get(path).copied().unwrap_or(1),
        "width": identity.and_then(|identity| identity.width),
        "height": identity.and_then(|identity| identity.height),
        "autotag": metadata.and_then(|metadata| metadata.autotag.as_ref()),
    })
}

pub(super) fn dynamic_playlist_item_json(
    entry: &PhotoEntry,
    metadata: Option<&ContentMetadata>,
) -> serde_json::Value {
    let path = entry.path.display().to_string();
    serde_json::json!({
        "path": path,
        "resolved_path": path,
        "content_id": format!("blake3:{}", entry.hash),
        "tag_groups": metadata.map(|metadata| &metadata.tag_groups).cloned().unwrap_or_default(),
        "rating": metadata.and_then(|metadata| metadata.rating),
        "frequency": 1,
        "width": entry.width,
        "height": entry.height,
        "autotag": metadata.and_then(|metadata| metadata.autotag.as_ref()),
    })
}

pub(super) fn content_item_json(
    index: &PhotoIndex,
    hash: &str,
    metadata: Option<&ContentMetadata>,
    identity: Option<&ResolvedContent>,
    probe_aliases: bool,
) -> serde_json::Value {
    let indexed: Vec<&PhotoEntry> = index
        .photos
        .iter()
        .filter(|entry| entry.hash == hash)
        .collect();
    let indexed_paths: Vec<String> = indexed
        .iter()
        .map(|entry| entry.path.display().to_string())
        .collect();
    let mut aliases = metadata
        .map(|metadata| metadata.aliases.clone())
        .unwrap_or_default();
    if let Some(identity) = identity {
        for alias in &identity.aliases {
            if !aliases.contains(alias) {
                aliases.push(alias.clone());
            }
        }
    }
    let available_aliases: Vec<String> = if probe_aliases {
        aliases
            .iter()
            .filter(|alias| Path::new(alias).is_file())
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let orphaned = if indexed.is_empty() {
        probe_aliases.then_some(available_aliases.is_empty())
    } else {
        Some(false)
    };
    let dimensions = metadata
        .and_then(|metadata| metadata.width.zip(metadata.height))
        .or_else(|| identity.and_then(|identity| identity.width.zip(identity.height)))
        .or_else(|| {
            indexed
                .iter()
                .find_map(|entry| entry.width.zip(entry.height))
        });
    let resolved_path = identity
        .filter(|identity| !identity.path.as_os_str().is_empty())
        .map(|identity| identity.path.display().to_string());

    serde_json::json!({
        "content_id": format!("blake3:{hash}"),
        "aliases": aliases,
        "available_aliases": available_aliases,
        "indexed_paths": indexed_paths,
        "orphaned": orphaned,
        "resolved_path": resolved_path,
        "tag_groups": metadata.map(|metadata| &metadata.tag_groups).cloned().unwrap_or_default(),
        "rating": metadata.and_then(|metadata| metadata.rating),
        "rating_conflicted": metadata.is_some_and(|metadata| metadata.rating_conflicted),
        "width": dimensions.map(|(width, _)| width),
        "height": dimensions.map(|(_, height)| height),
        "autotag": metadata.and_then(|metadata| metadata.autotag.as_ref()),
    })
}

pub(super) fn content_list_result_json(
    total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    items: &[serde_json::Value],
) -> serde_json::Value {
    serde_json::json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "next_offset": next_offset,
        "items": items,
    })
}

pub(super) fn bounded_content_list(
    total: usize,
    offset: usize,
    limit: usize,
    mut item_at: impl FnMut(usize) -> anyhow::Result<serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    let mut items = Vec::new();
    let mut overflow_at = None;
    let end = total.min(offset.saturating_add(limit));

    for index in offset..end {
        items.push(item_at(index)?);
        let next = offset.saturating_add(items.len());
        let candidate =
            content_list_result_json(total, offset, limit, (next < total).then_some(next), &items);
        if playlist_show_fits_frame(&candidate)? {
            continue;
        }
        let item = items.pop().expect("the candidate contains the new item");
        let single_next = index.saturating_add(1);
        let single = content_list_result_json(
            total,
            index,
            1,
            (single_next < total).then_some(single_next),
            std::slice::from_ref(&item),
        );
        if !playlist_show_fits_frame(&single)? {
            anyhow::bail!(
                "content item at offset {index} exceeds the IPC response limit; reduce its autotag metadata before retrying"
            );
        }
        if items.is_empty() {
            anyhow::bail!(
                "content item at offset {index} does not fit with limit {limit}; retry with --limit 1 or reduce its autotag metadata"
            );
        }
        overflow_at = Some(index);
        break;
    }

    let next = offset.saturating_add(items.len());
    let next_offset = overflow_at.or_else(|| (next < total).then_some(next));
    let result = content_list_result_json(total, offset, limit, next_offset, &items);
    if !playlist_show_fits_frame(&result)? {
        anyhow::bail!("content page exceeds the IPC response limit");
    }
    Ok(result)
}

pub(super) fn playlist_show_result_json(
    summary: &serde_json::Value,
    total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    items: &[serde_json::Value],
) -> serde_json::Value {
    serde_json::json!({
        "playlist": summary,
        "total": total,
        "offset": offset,
        "limit": limit,
        "next_offset": next_offset,
        "items": items,
    })
}

pub(super) fn playlist_show_wire_len(result: &serde_json::Value) -> anyhow::Result<usize> {
    Ok(serde_json::to_vec(&serde_json::json!({
        "success": true,
        "result": result,
    }))?
    .len())
}

pub(super) fn playlist_show_fits_frame(result: &serde_json::Value) -> anyhow::Result<bool> {
    Ok(playlist_show_wire_len(result)? <= MAX_FRAME_SIZE)
}

pub(super) fn bounded_playlist_show(
    summary: &serde_json::Value,
    total: usize,
    offset: usize,
    limit: usize,
    mut item_at: impl FnMut(usize) -> anyhow::Result<serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    let mut items = Vec::new();
    let mut overflow_at = None;
    let end = total.min(offset.saturating_add(limit));

    for index in offset..end {
        items.push(item_at(index)?);
        let next = offset.saturating_add(items.len());
        let candidate = playlist_show_result_json(
            summary,
            total,
            offset,
            limit,
            (next < total).then_some(next),
            &items,
        );
        if playlist_show_fits_frame(&candidate)? {
            continue;
        }

        let item = items.pop().expect("the candidate contains the new item");
        let single_next = index.saturating_add(1);
        let single = playlist_show_result_json(
            summary,
            total,
            index,
            1,
            (single_next < total).then_some(single_next),
            std::slice::from_ref(&item),
        );
        if !playlist_show_fits_frame(&single)? {
            anyhow::bail!(
                "playlist item at offset {index} exceeds the IPC response limit; reduce its tag metadata before retrying"
            );
        }
        if items.is_empty() {
            anyhow::bail!(
                "playlist item at offset {index} does not fit with limit {limit}; retry with --limit 1 or reduce its tag metadata"
            );
        }
        overflow_at = Some(index);
        break;
    }

    let next = offset.saturating_add(items.len());
    let next_offset = overflow_at.or_else(|| (next < total).then_some(next));
    let result = playlist_show_result_json(summary, total, offset, limit, next_offset, &items);
    if !playlist_show_fits_frame(&result)? {
        anyhow::bail!("playlist summary exceeds the IPC response limit; shorten the playlist name");
    }
    Ok(result)
}

pub(super) fn playlist_path_has_autotag_metadata(
    store: &PlaylistStore,
    name: &str,
    path: &str,
) -> bool {
    let Some(playlist) = store.get(name) else {
        return false;
    };
    playlist.ratings.contains_key(path)
        || playlist.frequencies.contains_key(path)
        || playlist
            .tag_groups
            .values()
            .any(|group| group.get(path).is_some_and(|tags| !tags.is_empty()))
}

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
    item_at: impl FnMut(usize) -> anyhow::Result<serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    bounded_page(
        PageText {
            noun: "content",
            reduce: "its autotag metadata",
            page_too_large: "content page exceeds the IPC response limit",
        },
        total,
        offset,
        limit,
        item_at,
        |offset, limit, next, items| content_list_result_json(total, offset, limit, next, items),
    )
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
    item_at: impl FnMut(usize) -> anyhow::Result<serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    bounded_page(
        PageText {
            noun: "playlist",
            reduce: "its tag metadata",
            page_too_large:
                "playlist summary exceeds the IPC response limit; shorten the playlist name",
        },
        total,
        offset,
        limit,
        item_at,
        |offset, limit, next, items| {
            playlist_show_result_json(summary, total, offset, limit, next, items)
        },
    )
}

/// Wording for [`bounded_page`] errors.
pub(super) struct PageText {
    noun: &'static str,
    reduce: &'static str,
    page_too_large: &'static str,
}

/// Slack for the `next_offset` field changing between the running estimate
/// and the final page (a number growing by a few digits, or null).
const PAGE_ESTIMATE_SLACK: usize = 64;

/// Collect up to `limit` items from `offset` into the largest page that fits
/// one IPC frame. Each item is serialized once and a running byte count
/// decides the cut, so a page costs O(items) instead of re-serializing the
/// whole page after every item.
pub(super) fn bounded_page(
    text: PageText,
    total: usize,
    offset: usize,
    limit: usize,
    mut item_at: impl FnMut(usize) -> anyhow::Result<serde_json::Value>,
    build: impl Fn(usize, usize, Option<usize>, &[serde_json::Value]) -> serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let fits = |value: &serde_json::Value| playlist_show_fits_frame(value);
    let base_len = playlist_show_wire_len(&build(offset, limit, Some(total), &[]))?;
    let mut used = base_len + PAGE_ESTIMATE_SLACK;
    let mut items = Vec::new();
    let mut overflow_at = None;
    let end = total.min(offset.saturating_add(limit));

    for index in offset..end {
        let item = item_at(index)?;
        // Item bytes plus the separating comma.
        let item_len = serde_json::to_vec(&item)?.len() + 1;
        if used + item_len <= MAX_FRAME_SIZE {
            used += item_len;
            items.push(item);
            continue;
        }
        let single_next = index.saturating_add(1);
        let single = build(
            index,
            1,
            (single_next < total).then_some(single_next),
            std::slice::from_ref(&item),
        );
        if !fits(&single)? {
            anyhow::bail!(
                "{} item at offset {index} exceeds the IPC response limit; reduce {} before retrying",
                text.noun,
                text.reduce
            );
        }
        if items.is_empty() {
            anyhow::bail!(
                "{} item at offset {index} does not fit with limit {limit}; retry with --limit 1 or reduce {}",
                text.noun,
                text.reduce
            );
        }
        overflow_at = Some(index);
        break;
    }

    let next = offset.saturating_add(items.len());
    let next_offset = overflow_at.or_else(|| (next < total).then_some(next));
    let result = build(offset, limit, next_offset, &items);
    if !fits(&result)? {
        anyhow::bail!("{}", text.page_too_large);
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

//! Crash-safe updates spanning `playlists.kdl` and `content.json`.

use super::*;

pub(super) const PLAYLIST_CONTENT_TRANSACTION_FILENAME: &str = "playlist-content.txn.json";

pub(super) const PLAYLIST_CONTENT_TRANSACTION_VERSION: u32 = 1;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlaylistContentTransaction {
    pub(super) version: u32,
    pub(super) content_json: String,
    pub(super) playlists_kdl: String,
}

pub(super) fn playlist_content_transaction_path(content_path: &Path) -> PathBuf {
    content_path.with_file_name(PLAYLIST_CONTENT_TRANSACTION_FILENAME)
}

pub(super) fn validate_playlist_content_consistency(
    playlists: &PlaylistStore,
    content: &ContentStore,
) -> Result<()> {
    for name in content.dynamic_playlists() {
        let playlist = playlists.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "content metadata marks playlist '{name}' dynamic, but playlists.kdl does not define it"
            )
        })?;
        if !playlist.paths.is_empty()
            || !playlist.tag_groups.is_empty()
            || !playlist.ratings.is_empty()
            || !playlist.frequencies.is_empty()
        {
            anyhow::bail!(
                "dynamic playlist '{name}' must not contain path membership or path-local metadata"
            );
        }
    }
    for name in content.playlist_filter_names() {
        if playlists.get(name).is_none() {
            anyhow::bail!(
                "content metadata has filters for missing playlist '{name}'; create it or remove the stale filter"
            );
        }
    }
    Ok(())
}

pub(super) fn validate_playlist_content_transaction(
    transaction: &PlaylistContentTransaction,
) -> Result<()> {
    if transaction.version != PLAYLIST_CONTENT_TRANSACTION_VERSION {
        anyhow::bail!(
            "unsupported playlist/content transaction version {}; expected {}",
            transaction.version,
            PLAYLIST_CONTENT_TRANSACTION_VERSION
        );
    }
    let content = parse_content(transaction.content_json.as_bytes())
        .context("validate transaction content metadata")?;
    let playlists =
        parse_playlists(&transaction.playlists_kdl).context("validate transaction playlists")?;
    validate_playlist_content_consistency(&playlists, &content)
        .context("validate transaction playlist/content consistency")
}

pub(super) fn stage_playlist_content_transaction(
    transaction: &PlaylistContentTransaction,
    playlists_path: &Path,
    content_path: &Path,
) -> Result<()> {
    write_synced(
        &content_path.with_extension("json.tmp"),
        transaction.content_json.as_bytes(),
    )?;
    write_synced(
        &playlists_path.with_extension("kdl.tmp"),
        transaction.playlists_kdl.as_bytes(),
    )
}

pub(super) fn install_playlist_content_transaction(
    playlists_path: &Path,
    content_path: &Path,
) -> Result<()> {
    crate::playlist::replace_file(&content_path.with_extension("json.tmp"), content_path)?;
    crate::playlist::replace_file(&playlists_path.with_extension("kdl.tmp"), playlists_path)
}

pub(super) fn remove_transaction_marker(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(
                "playlist/content transaction committed but marker {} could not be removed: {error}",
                path.display()
            );
        }
    }
}

pub(super) fn recover_playlist_content_transaction(
    playlists_path: &Path,
    content_path: &Path,
) -> Result<()> {
    let transaction_path = playlist_content_transaction_path(content_path);
    if !transaction_path.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(&transaction_path)
        .with_context(|| format!("read transaction {}", transaction_path.display()))?;
    let transaction: PlaylistContentTransaction = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse transaction {}", transaction_path.display()))?;
    validate_playlist_content_transaction(&transaction)?;
    stage_playlist_content_transaction(&transaction, playlists_path, content_path)?;
    install_playlist_content_transaction(playlists_path, content_path)?;
    remove_transaction_marker(&transaction_path);
    info!(
        "recovered committed playlist/content transaction {}",
        transaction_path.display()
    );
    Ok(())
}

pub(super) fn commit_playlist_content_transaction(
    playlists: &PlaylistStore,
    content: &ContentStore,
    playlists_path: &Path,
    content_path: &Path,
) -> Result<()> {
    validate_playlist_content_consistency(playlists, content)?;
    let transaction = PlaylistContentTransaction {
        version: PLAYLIST_CONTENT_TRANSACTION_VERSION,
        content_json: String::from_utf8(serialize_content(content)?)
            .context("content metadata serialization was not UTF-8")?,
        playlists_kdl: serialize_playlists_checked(playlists)?,
    };
    stage_playlist_content_transaction(&transaction, playlists_path, content_path)?;

    let transaction_path = playlist_content_transaction_path(content_path);
    let transaction_tmp = transaction_path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec(&transaction).context("serialize playlist/content transaction")?;
    write_synced(&transaction_tmp, &bytes)?;
    crate::playlist::replace_file(&transaction_tmp, &transaction_path)?;

    if let Err(error) = install_playlist_content_transaction(playlists_path, content_path) {
        warn!(
            "playlist/content transaction {} is committed and will be recovered later: {error:#}",
            transaction_path.display()
        );
        return Ok(());
    }
    remove_transaction_marker(&transaction_path);
    Ok(())
}

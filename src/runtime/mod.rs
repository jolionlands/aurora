//! Aurora's runtime orchestrator: receives SwapRequests from the scheduler,
//! picks a photo from the index, decodes it (with cache), runs the configured
//! transition, applies the new wallpaper via IDesktopWallpaper, updates metrics.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::apply::{configured_global_fit, MonitorSnapshot};
use crate::config::types::{Config, DEFAULT_IMAGE_EXTENSIONS};
use crate::content::{
    content_path, load_content, parse_content, persist_content, serialize_content,
    AutoTagProvenance, ContentMetadata, ContentStore, TagFilters,
};
use crate::decode::SharedDecodeCache;
use crate::index::{PhotoEntry, PhotoIndex};
use crate::ipc::messages::{IpcEvent, MAX_CONTENT_LIST_LIMIT, MAX_PLAYLIST_SHOW_LIMIT};
use crate::ipc::MAX_FRAME_SIZE;
use crate::metrics::Metrics;
use crate::playlist::{
    default_playlists_path, load_playlists, parse_playlists, persist_playlists, pick_weighted_path,
    serialize_playlists_checked, write_synced, Playlist, PlaylistStore,
};
use crate::scheduler::{checked_pause_deadline, SchedulerProgress, SwapReason, SwapRequest};
use crate::transition::{Backend, Rect, TransitionRenderer, TransitionStyle};

// ---------------------------------------------------------------------------
// RuntimeState
// ---------------------------------------------------------------------------

pub struct RuntimeState {
    /// Current wallpaper path per monitor ID.
    pub current_path: HashMap<String, PathBuf>,
    /// History ring for `aurora-ctl prev` (most-recent at back).
    pub history: VecDeque<PathBuf>,
    /// Recent paths for anti-repeat window.
    pub recent_paths: VecDeque<PathBuf>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            current_path: HashMap::new(),
            history: VecDeque::new(),
            recent_paths: VecDeque::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

pub struct Runtime {
    index: Arc<RwLock<PhotoIndex>>,
    ban_gate: BanGate,
    source_roots: Arc<RwLock<Vec<PathBuf>>>,
    cache: SharedDecodeCache,
    transitions: TransitionRenderer,
    metrics: Arc<Metrics>,
    state: RuntimeState,
    config: Config,
    scheduler_progress: SchedulerProgress,
    event_tx: Option<tokio::sync::broadcast::Sender<IpcEvent>>,
    /// Shared playlist store — also held by RuntimeHandle for IPC mutations.
    playlist_store: Arc<Mutex<PlaylistStore>>,
    content_store: Arc<Mutex<ContentStore>>,
    /// Sequential cursor: playlist_name → next_index.
    playlist_cursor: std::collections::HashMap<String, usize>,
}

const HISTORY_CAP: usize = 50;
const BYTES_PER_4K_BGRA: usize = 3840 * 2160 * 4;
const BANS_FILENAME: &str = "bans.txt";
const INDEX_CACHE_FILENAME: &str = "index-cache.json";
const PLAYLIST_CONTENT_TRANSACTION_FILENAME: &str = "playlist-content.txn.json";
const PLAYLIST_CONTENT_TRANSACTION_VERSION: u32 = 1;
const DIRECT_APPLY_TIMEOUT: Duration = Duration::from_secs(10);
const ALL_MONITORS_ID: &str = "all";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistContentTransaction {
    version: u32,
    content_json: String,
    playlists_kdl: String,
}

#[derive(Default)]
struct BanCoordinator {
    updates: Mutex<()>,
    hashes: RwLock<HashSet<String>>,
}

/// Shared synchronization point for ban persistence and the final wallpaper apply.
#[derive(Clone, Default)]
pub struct BanGate(Arc<BanCoordinator>);

#[derive(Clone)]
pub struct RuntimeShared {
    index: Arc<RwLock<PhotoIndex>>,
    source_roots: Arc<RwLock<Vec<PathBuf>>>,
    ban_gate: BanGate,
    content_store: Arc<Mutex<ContentStore>>,
}

impl RuntimeShared {
    pub fn new(
        index: Arc<RwLock<PhotoIndex>>,
        source_roots: Arc<RwLock<Vec<PathBuf>>>,
        ban_gate: BanGate,
        content_store: Arc<Mutex<ContentStore>>,
    ) -> Self {
        Self {
            index,
            source_roots,
            ban_gate,
            content_store,
        }
    }
}

impl BanGate {
    fn new(hashes: HashSet<String>) -> Self {
        Self(Arc::new(BanCoordinator {
            updates: Mutex::new(()),
            hashes: RwLock::new(hashes),
        }))
    }

    fn is_banned(&self, hash: &str) -> bool {
        self.0.hashes.read().contains(hash)
    }

    fn run_if_allowed<T>(
        &self,
        hash: &str,
        apply: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        let hashes = self.0.hashes.read();
        if hashes.contains(hash) {
            return Ok(None);
        }
        let result = apply().map(Some);
        drop(hashes);
        result
    }
}

pub struct ComApartment {
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ComApartment {
    pub fn initialize() -> Result<Self> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_err() {
            anyhow::bail!("CoInitializeEx failed: {result:?}");
        }
        Ok(Self {
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

fn cache_budget_bytes(decoded_mb: u32) -> usize {
    let bytes = u64::from(decoded_mb).saturating_mul(1024 * 1024);
    bytes.min(usize::MAX as u64) as usize
}

fn cache_capacity(decoded_bytes: usize) -> usize {
    (decoded_bytes / BYTES_PER_4K_BGRA).max(1)
}

fn monitor_results(successful: usize, failures: &[String]) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    let message = format!(
        "wallpaper updated on {successful} monitor(s), failed on {}: {}",
        failures.len(),
        failures.join("; ")
    );
    if successful > 0 {
        warn!("{message}");
        return Ok(());
    }
    Err(anyhow::anyhow!(message))
}

fn needs_transition_decode(enabled: bool, has_previous: bool) -> bool {
    enabled && has_previous
}

fn apply_direct_in_child(path: &Path, fit: Option<&str>, monitor_id: Option<&str>) -> Result<()> {
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    let mut command = Command::new(std::env::current_exe().context("locate aurora executable")?);
    command.arg("--apply-once").arg(path);
    if let Some(fit) = fit {
        command.arg("--apply-fit").arg(fit);
    }
    if let Some(monitor_id) = monitor_id {
        command.arg("--apply-monitor").arg(monitor_id);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0);
    let child = command.spawn().context("start wallpaper apply helper")?;
    let output = wait_for_helper_child(child, DIRECT_APPLY_TIMEOUT)?;
    if !output.status.success() {
        anyhow::bail!(
            "wallpaper apply helper failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn inspect_wallpapers_in_child() -> Result<Vec<MonitorSnapshot>> {
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    let mut command = Command::new(std::env::current_exe().context("locate aurora executable")?);
    command
        .arg("--inspect-wallpapers")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0);
    let child = command
        .spawn()
        .context("start wallpaper inspection helper")?;
    let output = wait_for_helper_child(child, DIRECT_APPLY_TIMEOUT)?;
    if !output.status.success() {
        anyhow::bail!(
            "wallpaper inspection helper failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("parse wallpaper inspection helper output")
}

fn wait_for_helper_child(mut child: Child, timeout: Duration) -> Result<Output> {
    let started = Instant::now();

    loop {
        if child
            .try_wait()
            .context("poll wallpaper apply helper")?
            .is_some()
        {
            return child
                .wait_with_output()
                .context("collect wallpaper apply helper output");
        }
        if started.elapsed() >= timeout {
            if let Err(error) = child.kill() {
                if child.try_wait().ok().flatten().is_none() {
                    anyhow::bail!(
                        "wallpaper helper timed out after {} milliseconds and could not be terminated: {error}",
                        timeout.as_millis()
                    );
                }
            } else {
                let _ = child.wait();
            }
            anyhow::bail!(
                "wallpaper helper timed out after {} milliseconds",
                timeout.as_millis()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn indexed_hash(index: &PhotoIndex, path: &Path) -> Option<String> {
    index
        .photos
        .iter()
        .find(|entry| entry.path == path)
        .map(|entry| entry.hash.clone())
}

fn target_hash(
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

fn banned_paths(index: &PhotoIndex) -> HashSet<PathBuf> {
    index
        .photos
        .iter()
        .filter(|entry| entry.banned)
        .map(|entry| entry.path.clone())
        .collect()
}

fn dynamic_playlist_entries<'a>(
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

fn dynamic_playlist_candidates(
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

fn rotation_target(
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

fn commit_successful_monitors(
    state: &mut RuntimeState,
    metrics: &Metrics,
    event_tx: Option<&tokio::sync::broadcast::Sender<IpcEvent>>,
    new_path: &Path,
    reason: &SwapReason,
    recent_window: usize,
    monitor_ids: &[String],
) {
    if monitor_ids.len() == 1 && monitor_ids[0] == ALL_MONITORS_ID {
        state.current_path.clear();
        metrics.current_photo.lock().clear();
    }
    for monitor_id in monitor_ids {
        state
            .current_path
            .insert(monitor_id.clone(), new_path.to_path_buf());
        metrics.set_current_photo(monitor_id, new_path.to_path_buf());

        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if let Some(tx) = event_tx {
            let _ = tx.send(IpcEvent::WallpaperChanged {
                monitor_id: monitor_id.clone(),
                path: new_path.display().to_string(),
            });
            let _ = tx.send(IpcEvent::Swapped {
                monitor: monitor_id.clone(),
                path: new_path.display().to_string(),
                ts_ms,
            });
        }
        debug!(
            "swapped monitor={} path={} ts_ms={}",
            monitor_id,
            new_path.display(),
            ts_ms
        );
    }

    if monitor_ids.is_empty() {
        return;
    }
    record_successful_history(&mut state.history, new_path, reason);
    state.recent_paths.push_back(new_path.to_path_buf());
    while state.recent_paths.len() > recent_window.max(1) {
        state.recent_paths.pop_front();
    }
    metrics.record_swap();
}

fn should_suppress_same_target(
    reason: &SwapReason,
    current_paths: &HashMap<String, PathBuf>,
    target: &Path,
    attached_monitor_ids: &[String],
) -> bool {
    if matches!(reason, SwapReason::Manual | SwapReason::Previous)
        || attached_monitor_ids.is_empty()
    {
        return false;
    }
    let Ok(target) = std::fs::canonicalize(target) else {
        return false;
    };
    attached_monitor_ids.iter().all(|monitor_id| {
        current_paths.get(monitor_id).is_some_and(|current| {
            std::fs::canonicalize(current).is_ok_and(|current| current == target)
        })
    })
}

fn reconcile_attached_monitors(
    state: &mut RuntimeState,
    metrics: &Metrics,
    attached_monitor_ids: &[String],
) {
    if let Some(path) = state.current_path.remove(ALL_MONITORS_ID) {
        metrics.current_photo.lock().remove(ALL_MONITORS_ID);
        for monitor_id in attached_monitor_ids {
            state
                .current_path
                .entry(monitor_id.clone())
                .or_insert_with(|| path.clone());
            metrics.set_current_photo(monitor_id, path.clone());
        }
    }
    let attached: HashSet<&str> = attached_monitor_ids.iter().map(String::as_str).collect();
    state
        .current_path
        .retain(|monitor_id, _| attached.contains(monitor_id.as_str()));
    metrics
        .current_photo
        .lock()
        .retain(|monitor_id, _| attached.contains(monitor_id.as_str()));
}

fn previous_path(history: &VecDeque<PathBuf>) -> Option<PathBuf> {
    history.iter().rev().nth(1).cloned()
}

fn record_successful_history(history: &mut VecDeque<PathBuf>, path: &Path, reason: &SwapReason) {
    if *reason == SwapReason::Previous {
        history.pop_back();
    } else {
        history.push_back(path.to_path_buf());
        if history.len() > HISTORY_CAP {
            history.pop_front();
        }
    }
}

fn bans_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name(BANS_FILENAME)
}

fn index_cache_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name(INDEX_CACHE_FILENAME)
}

fn playlist_content_transaction_path(content_path: &Path) -> PathBuf {
    content_path.with_file_name(PLAYLIST_CONTENT_TRANSACTION_FILENAME)
}

fn validate_playlist_content_consistency(
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

fn validate_playlist_content_transaction(transaction: &PlaylistContentTransaction) -> Result<()> {
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

fn stage_playlist_content_transaction(
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

fn install_playlist_content_transaction(playlists_path: &Path, content_path: &Path) -> Result<()> {
    crate::playlist::replace_file(&content_path.with_extension("json.tmp"), content_path)?;
    crate::playlist::replace_file(&playlists_path.with_extension("kdl.tmp"), playlists_path)
}

fn remove_transaction_marker(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(
                "playlist/content transaction committed but marker {} could not be removed: {error}",
                path.display()
            );
        }
    }
}

fn recover_playlist_content_transaction(playlists_path: &Path, content_path: &Path) -> Result<()> {
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

fn commit_playlist_content_transaction(
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

fn normalize_ban_hash(hash: &str) -> Result<String> {
    let hash = hash.trim();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("ban hash must be exactly 64 hexadecimal characters");
    }
    Ok(hash.to_ascii_lowercase())
}

fn load_bans(path: &Path) -> Result<HashSet<String>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("read bans sidecar {}", path.display()))?;
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            normalize_ban_hash(line)
                .with_context(|| format!("bans sidecar {} line {}", path.display(), index + 1))
        })
        .collect()
}

fn persist_bans(path: &Path, bans: &HashSet<String>) -> Result<()> {
    let mut hashes: Vec<&str> = bans.iter().map(String::as_str).collect();
    hashes.sort_unstable();
    let mut content = hashes.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    let tmp = path.with_extension("txt.tmp");
    write_synced(&tmp, content.as_bytes())?;
    crate::playlist::replace_file(&tmp, path)
}

fn resolved_playlist_path(stored: &str, source_roots: &[PathBuf]) -> Option<PathBuf> {
    let path = PathBuf::from(stored);
    if path.is_absolute() || source_roots.is_empty() {
        return path.is_file().then_some(path);
    }
    source_roots
        .iter()
        .map(|root| root.join(&path))
        .find(|candidate| candidate.is_file())
}

fn seed_recent_current_paths(state: &mut RuntimeState) {
    let mut seen = HashSet::new();
    for path in state.current_path.values() {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if seen.insert(path.clone()) {
            state.recent_paths.push_back(path);
        }
    }
}

fn runtime_state_from_snapshots(snapshots: Vec<MonitorSnapshot>) -> (RuntimeState, bool) {
    let mut state = RuntimeState::new();
    let expected_monitors = snapshots.len();
    for snapshot in snapshots {
        if let Some(path) = snapshot.current_path.filter(|path| path.is_file()) {
            state.current_path.insert(snapshot.monitor.id, path);
        }
    }
    seed_recent_current_paths(&mut state);
    let complete = expected_monitors > 0 && state.current_path.len() == expected_monitors;
    (state, complete)
}

fn initial_runtime_state() -> (RuntimeState, bool) {
    match inspect_wallpapers_in_child() {
        Ok(snapshots) => runtime_state_from_snapshots(snapshots),
        Err(error) => {
            warn!("could not seed current wallpapers: {error:#}");
            (RuntimeState::new(), false)
        }
    }
}

fn migrate_legacy_content(
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

fn playlist_path_tag_groups(playlist: &Playlist, path: &str) -> BTreeMap<String, Vec<String>> {
    playlist
        .tag_groups
        .iter()
        .filter_map(|(kind, paths)| paths.get(path).cloned().map(|tags| (kind.clone(), tags)))
        .collect()
}

fn effective_tag_groups(
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
struct ResolvedContent {
    hash: String,
    path: PathBuf,
    aliases: Vec<String>,
    width: Option<u32>,
    height: Option<u32>,
}

fn indexed_entry_for_path<'a>(index: &'a PhotoIndex, path: &Path) -> Option<&'a PhotoEntry> {
    if let Some(entry) = index.photos.iter().find(|entry| entry.path == path) {
        return Some(entry);
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    index.photos.iter().find(|entry| {
        std::fs::canonicalize(&entry.path).is_ok_and(|entry_path| entry_path == canonical)
    })
}

fn resolve_content(
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

fn content_id_hash(target: &str) -> Result<Option<String>> {
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

fn resolved_content_by_hash(
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

fn resolve_content_target(
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

impl Runtime {
    pub fn new(
        config: &Config,
        config_path: &Path,
        metrics: Arc<Metrics>,
        scheduler_progress: SchedulerProgress,
    ) -> Result<Self> {
        let playlists_path = default_playlists_path();
        let metadata_path = content_path(config_path);
        recover_playlist_content_transaction(&playlists_path, &metadata_path)
            .context("recover playlist/content transaction during startup")?;

        // Build photo index from configured sources.
        let mut index = if config.sources.is_empty() {
            PhotoIndex::default()
        } else {
            PhotoIndex::scan_sources_cached(&config.sources, &index_cache_path(config_path))
                .context("scanning photo sources")?
        };
        let persisted_bans = load_bans(&bans_path(config_path))?;
        let banned_count = index.apply_bans(&persisted_bans);
        let ban_gate = BanGate::new(persisted_bans);

        let index_size = index.len() as u64;
        metrics.set_index_size(index_size);
        info!(
            "photo index built: {} photos ({} banned)",
            index_size, banned_count
        );

        let style = TransitionStyle::parse(&config.transitions.style);
        let backend = Backend::parse(&config.transitions.renderer);
        let transitions = TransitionRenderer::new(style, config.transitions.duration_ms, backend);

        let configured_cache_bytes = cache_budget_bytes(config.cache.decoded_mb);
        let cache_capacity = cache_capacity(configured_cache_bytes);
        info!(
            "decode cache capacity: {} entries (~{} MB budget)",
            cache_capacity, config.cache.decoded_mb
        );
        let cache = SharedDecodeCache::with_byte_budget(
            cache_capacity,
            configured_cache_bytes,
            Arc::clone(&metrics),
        );

        // Load playlist store from disk (creates empty default if file is absent).
        let playlist_store = load_playlists(&playlists_path)
            .with_context(|| format!("load playlists {}", playlists_path.display()))?;
        let source_roots: Vec<PathBuf> = config
            .sources
            .iter()
            .map(|source| source.path.clone())
            .collect();
        let mut content_store = load_content(&metadata_path)
            .with_context(|| format!("load content metadata {}", metadata_path.display()))?;
        validate_playlist_content_consistency(&playlist_store, &content_store)
            .context("validate loaded playlist/content metadata")?;
        if migrate_legacy_content(&mut content_store, &playlist_store, &index, &source_roots)? {
            persist_content(&content_store, &metadata_path).with_context(|| {
                format!(
                    "persist migrated content metadata {}",
                    metadata_path.display()
                )
            })?;
        }

        let (state, complete_monitor_snapshot) = initial_runtime_state();
        if complete_monitor_snapshot {
            scheduler_progress.seed_success();
        }
        for (monitor, path) in &state.current_path {
            metrics.set_current_photo(monitor, path.clone());
        }
        Ok(Self {
            index: Arc::new(RwLock::new(index)),
            ban_gate,
            source_roots: Arc::new(RwLock::new(source_roots)),
            cache,
            transitions,
            metrics,
            state,
            config: config.clone(),
            scheduler_progress,
            event_tx: None,
            playlist_store: Arc::new(Mutex::new(playlist_store)),
            content_store: Arc::new(Mutex::new(content_store)),
            playlist_cursor: std::collections::HashMap::new(),
        })
    }

    /// Wire the IPC broadcast sender so Runtime can emit WallpaperChanged events.
    pub fn set_event_sender(&mut self, tx: tokio::sync::broadcast::Sender<IpcEvent>) {
        self.event_tx = Some(tx);
    }

    pub fn shared(&self) -> RuntimeShared {
        RuntimeShared::new(
            Arc::clone(&self.index),
            Arc::clone(&self.source_roots),
            self.ban_gate.clone(),
            Arc::clone(&self.content_store),
        )
    }

    pub fn state_snapshot(&self) -> RuntimeStateSnapshot {
        RuntimeStateSnapshot {
            current_path: self.state.current_path.clone(),
            history: self.state.history.clone(),
        }
    }

    /// Expose the playlist store Arc so main can hand it to RuntimeHandle.
    pub fn playlist_arc(&self) -> Arc<Mutex<PlaylistStore>> {
        Arc::clone(&self.playlist_store)
    }

    /// Consume the runtime, processing SwapRequests until the channel closes.
    ///
    /// `handle_state` is written after each swap so IPC can read status.
    /// `pause_arc`    is written by IPC pause/resume commands and checked here
    ///                before each swap.
    pub async fn run(
        mut self,
        mut rx: mpsc::Receiver<SwapRequest>,
        handle_state: Arc<Mutex<RuntimeStateSnapshot>>,
        pause_arc: Arc<Mutex<PauseState>>,
    ) {
        while let Some(req) = rx.recv().await {
            let reason = req.reason.clone();
            if !self.scheduler_progress.should_process(&reason) {
                debug!("runtime: dropping automatic swap superseded by a newer successful change");
                continue;
            }
            {
                let mut p = pause_arc.lock();
                if p.blocks(&req.reason) {
                    debug!(
                        "runtime paused — dropping automatic swap request {:?}",
                        req.reason
                    );
                    self.scheduler_progress.defer(&reason);
                    continue;
                }
            }
            let result = self.handle_swap(req);
            self.scheduler_progress.complete(&reason, result.is_ok());
            if let Err(error) = result {
                warn!("swap failed: {}", error);
            }
            // Sync shared state snapshot for IPC queries.
            {
                let mut snap = handle_state.lock();
                snap.current_path = self.state.current_path.clone();
                snap.history = self.state.history.clone();
            }
        }
        info!("runtime: swap channel closed — exiting");
    }

    fn handle_swap(&mut self, req: SwapRequest) -> Result<()> {
        // Pick target path and retain the already-indexed hash when available.
        let (new_path, known_hash) = if req.reason == SwapReason::Previous {
            (
                previous_path(&self.state.history)
                    .ok_or_else(|| anyhow::anyhow!("no previous photo in history"))?,
                None,
            )
        } else if let Some(specific) = req.specific {
            (specific, None)
        } else {
            // When a playlist is active, pick from it; otherwise use the full index.
            // A rotation should never immediately re-select the wallpaper that
            // is already visible, even when the configurable history window is zero.
            let recent_window = self.config.schedule.min_repeat_window.max(1);
            let mut excluded_paths = banned_paths(&self.index.read());
            // Known index entries are filtered without I/O. If a playlist path
            // is outside the index, hash only selected rejected entries and retry.
            // ponytail: build a path→hash map only if unindexed bans become hot.
            let (playlist_active, playlist_pick) = loop {
                // Match source replacement's index-then-roots lock order.
                let (active, pick) = {
                    let index = self.index.read();
                    let source_roots = self.source_roots.read();
                    let store = self.playlist_store.lock();
                    let content = self.content_store.lock();
                    let active = store.active.is_some();
                    let pick = store.active_playlist().and_then(|playlist| {
                        if content.is_dynamic_playlist(&playlist.name) {
                            let candidates = dynamic_playlist_candidates(
                                &index,
                                &content,
                                content.playlist_filters(&playlist.name),
                                &excluded_paths,
                            );
                            pick_weighted_path(
                                &playlist.name,
                                playlist.shuffle,
                                &candidates,
                                &mut self.playlist_cursor,
                                recent_window,
                                &self.state.recent_paths,
                            )
                        } else {
                            store.pick_resolved(
                                &mut self.playlist_cursor,
                                recent_window,
                                &self.state.recent_paths,
                                &excluded_paths,
                                |playlist, stored| {
                                    let identity = resolve_content(
                                        &index,
                                        &content,
                                        stored,
                                        &source_roots,
                                        false,
                                    )
                                    .ok()
                                    .flatten();
                                    if let Some(identity) = identity {
                                        let metadata = content.get(&identity.hash);
                                        if content.playlist_filters(&playlist.name).is_some() {
                                            let groups = effective_tag_groups(
                                                playlist,
                                                stored,
                                                metadata,
                                                content.is_legacy_pending(&playlist.name, stored),
                                            );
                                            if !content
                                                .playlist_accepts(&playlist.name, Some(&groups))
                                            {
                                                return None;
                                            }
                                        }
                                        let rating = metadata.and_then(|metadata| metadata.rating);
                                        return Some((identity.path, rating));
                                    }
                                    let groups = playlist_path_tag_groups(playlist, stored);
                                    if content.playlist_accepts(&playlist.name, Some(&groups)) {
                                        resolved_playlist_path(stored, &source_roots)
                                            .map(|path| (path, None))
                                    } else {
                                        None
                                    }
                                },
                            )
                        }
                    });
                    (active, pick)
                };
                let Some(path) = pick else {
                    break (active, None);
                };
                let hash = target_hash(&self.index, &path, None)?;
                if !self.ban_gate.is_banned(&hash) {
                    break (active, Some((path, hash)));
                }
                excluded_paths.insert(path);
            };

            rotation_target(
                &self.index.read(),
                playlist_active,
                playlist_pick,
                recent_window,
                &self.state.recent_paths,
            )?
        };
        let target_hash = target_hash(&self.index, &new_path, known_hash)?;
        if self.ban_gate.is_banned(&target_hash) {
            anyhow::bail!("wallpaper is banned: {}", new_path.display());
        }

        let monitors = match inspect_wallpapers_in_child() {
            Ok(snapshots) if !snapshots.is_empty() => Some(
                snapshots
                    .into_iter()
                    .map(|snapshot| snapshot.monitor)
                    .collect::<Vec<_>>(),
            ),
            Ok(_) if self.config.transitions.enabled => {
                anyhow::bail!("no attached monitors found via IDesktopWallpaper")
            }
            Ok(_) => None,
            Err(error) if self.config.transitions.enabled => {
                return Err(error).context("listing monitors");
            }
            Err(error) => {
                warn!(
                    "could not enumerate monitors; falling back to one all-monitor commit: {error:#}"
                );
                None
            }
        };
        let attached_monitor_ids: Vec<String> = monitors
            .as_ref()
            .into_iter()
            .flat_map(|monitors| monitors.iter().map(|monitor| monitor.id.clone()))
            .collect();
        if !attached_monitor_ids.is_empty() {
            reconcile_attached_monitors(&mut self.state, &self.metrics, &attached_monitor_ids);
        }
        if should_suppress_same_target(
            &req.reason,
            &self.state.current_path,
            &new_path,
            &attached_monitor_ids,
        ) {
            debug!(
                path = %new_path.display(),
                reason = ?req.reason,
                "wallpaper already active on every attached monitor; suppressing redundant apply"
            );
            return Ok(());
        }

        let (successful_monitors, failures, total_monitors) = if let Some(monitors) = monitors {
            let fit = configured_global_fit(&self.config, &monitors).as_str();

            let mut successful_monitors = Vec::new();
            let mut failures = Vec::new();
            for monitor in &monitors {
                let (tw, th) = (monitor.width, monitor.height);
                let prev_path = self.state.current_path.get(&monitor.id).cloned();
                let transition_images = if needs_transition_decode(
                    self.config.transitions.enabled,
                    prev_path.is_some(),
                ) {
                    let t0 = std::time::Instant::now();
                    let new_decoded = self.cache.get_or_decode(&new_path, tw, th);
                    self.metrics
                        .record_decode_ms(t0.elapsed().as_millis() as u64);
                    let new_decoded = match new_decoded {
                        Ok(image) => image,
                        Err(error) => {
                            failures.push(format!(
                                "monitor {}: decode {}: {error:#}",
                                monitor.id,
                                new_path.display()
                            ));
                            continue;
                        }
                    };
                    prev_path
                        .as_ref()
                        .and_then(|old_path| self.cache.get_or_decode(old_path, tw, th).ok())
                        .map(|old_decoded| (old_decoded, new_decoded))
                } else {
                    None
                };

                // Gate the visible transition and COM commit together. A ban writer
                // cannot acknowledge while either is still showing this target.
                match self.ban_gate.run_if_allowed(&target_hash, || {
                    if let Some((old_decoded, new_decoded)) = &transition_images {
                        let bounds = Rect {
                            x: monitor.x,
                            y: monitor.y,
                            width: monitor.width,
                            height: monitor.height,
                        };
                        let committed = std::cell::Cell::new(false);
                        match self.transitions.run_with_commit(
                            bounds,
                            old_decoded,
                            new_decoded,
                            || {
                                apply_direct_in_child(&new_path, Some(fit), Some(&monitor.id))?;
                                committed.set(true);
                                Ok(())
                            },
                        ) {
                            Ok(()) => return Ok(()),
                            Err(error) if committed.get() => {
                                warn!(
                                    %error,
                                    "transition failed after wallpaper commit; keeping committed wallpaper"
                                );
                                return Ok(());
                            }
                            Err(error) => {
                                warn!(%error, "transition failed; continuing with direct apply");
                            }
                        }
                    }
                    apply_direct_in_child(&new_path, Some(fit), Some(&monitor.id))
                }) {
                    Ok(Some(())) => successful_monitors.push(monitor.id.clone()),
                    Ok(None) => {
                        failures.push(format!(
                            "monitor {}: wallpaper was banned during swap: {}",
                            monitor.id,
                            new_path.display()
                        ));
                        break;
                    }
                    Err(error) => {
                        failures.push(format!("monitor {}: {error:#}", monitor.id));
                        continue;
                    }
                }
            }
            let total_monitors = monitors.len();
            (successful_monitors, failures, total_monitors)
        } else {
            let fit = self
                .config
                .monitors
                .first()
                .map(|monitor| monitor.fit.as_str());
            self.ban_gate
                .run_if_allowed(&target_hash, || apply_direct_in_child(&new_path, fit, None))?
                .ok_or_else(|| {
                    anyhow::anyhow!("wallpaper was banned during swap: {}", new_path.display())
                })?;
            (vec![ALL_MONITORS_ID.to_string()], Vec::new(), 1)
        };

        let successful = successful_monitors.len();
        commit_successful_monitors(
            &mut self.state,
            &self.metrics,
            self.event_tx.as_ref(),
            &new_path,
            &req.reason,
            self.config.schedule.min_repeat_window,
            &successful_monitors,
        );
        if successful > 0 {
            info!(
                "wallpaper swapped → {} on {}/{} monitor(s) (reason={:?})",
                new_path.display(),
                successful,
                total_monitors,
                req.reason
            );
        }

        monitor_results(successful, &failures)
    }
}

// ---------------------------------------------------------------------------
// RuntimeHandle — clone-friendly, used by IPC
// ---------------------------------------------------------------------------

/// Shared snapshot of RuntimeState, updated after each swap.
#[derive(Default)]
pub struct RuntimeStateSnapshot {
    pub current_path: HashMap<String, PathBuf>,
    /// Full history ring, mirrored from Runtime::state so IPC `prev` can read it.
    pub history: VecDeque<PathBuf>,
}

/// A lightweight, Clone handle that IPC commands dispatch through.
#[derive(Clone)]
pub struct RuntimeHandle {
    swap_tx: mpsc::Sender<SwapRequest>,
    /// Shared with the Runtime::run loop for read-only status queries.
    pub(crate) state: Arc<Mutex<RuntimeStateSnapshot>>,
    pub(crate) index: Arc<RwLock<PhotoIndex>>,
    /// Effective roots used to resolve relative playlist entries.
    source_roots: Arc<RwLock<Vec<PathBuf>>>,
    pub(crate) metrics: Arc<Metrics>,
    /// Pause state is managed separately so IPC can set it without going
    /// through the swap channel (which would need runtime to drain it).
    paused: Arc<Mutex<PauseState>>,
    /// Path to the config file on disk, used by reload_from_disk().
    config_path: Arc<std::path::PathBuf>,
    /// Serializes source scans without delaying unrelated ban updates.
    source_updates: Arc<Mutex<()>>,
    /// Serializes playlist/content recovery and persistence.
    persistence: Arc<Mutex<()>>,
    /// Serializes ban persistence and gates the final wallpaper apply.
    ban_gate: BanGate,
    /// Shared playlist store.  IPC commands mutate this and persist to disk.
    pub(crate) playlist_store: Arc<Mutex<PlaylistStore>>,
    /// Shared metadata keyed by exact image content hash.
    pub(crate) content_store: Arc<Mutex<ContentStore>>,
    /// Path to the playlists KDL file on disk.
    playlists_path: Arc<std::path::PathBuf>,
    /// Path to the versioned content metadata sidecar.
    content_path: Arc<std::path::PathBuf>,
}

pub struct PauseState {
    pub paused: bool,
    pub pause_until: Option<Instant>,
}

impl PauseState {
    fn is_paused(&mut self) -> bool {
        if self
            .pause_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.paused = false;
            self.pause_until = None;
        }
        self.paused
    }

    fn blocks(&mut self, reason: &SwapReason) -> bool {
        self.is_paused() && !matches!(reason, SwapReason::Manual | SwapReason::Previous)
    }
}

fn validate_autotag_target(name: &str, path: &str) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("playlist name must not be empty");
    }
    if path.trim().is_empty() {
        anyhow::bail!("playlist path must not be empty");
    }
    Ok(())
}

fn playlist_summary_json(
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

fn playlist_item_json(
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

fn dynamic_playlist_item_json(
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

fn content_item_json(
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

fn content_list_result_json(
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

fn bounded_content_list(
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

fn playlist_show_result_json(
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

fn playlist_show_wire_len(result: &serde_json::Value) -> anyhow::Result<usize> {
    Ok(serde_json::to_vec(&serde_json::json!({
        "success": true,
        "result": result,
    }))?
    .len())
}

fn playlist_show_fits_frame(result: &serde_json::Value) -> anyhow::Result<bool> {
    Ok(playlist_show_wire_len(result)? <= MAX_FRAME_SIZE)
}

fn bounded_playlist_show(
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

fn playlist_path_has_autotag_metadata(store: &PlaylistStore, name: &str, path: &str) -> bool {
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

fn resolve_playlist_entry_path(
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

impl RuntimeHandle {
    pub fn new(
        swap_tx: mpsc::Sender<SwapRequest>,
        state: Arc<Mutex<RuntimeStateSnapshot>>,
        shared: RuntimeShared,
        metrics: Arc<Metrics>,
        config_path: std::path::PathBuf,
        playlist_store: Arc<Mutex<PlaylistStore>>,
        playlists_path: std::path::PathBuf,
    ) -> Self {
        let metadata_path = content_path(&config_path);
        Self {
            swap_tx,
            state,
            index: shared.index,
            source_roots: shared.source_roots,
            metrics,
            paused: Arc::new(Mutex::new(PauseState {
                paused: false,
                pause_until: None,
            })),
            config_path: Arc::new(config_path),
            source_updates: Arc::new(Mutex::new(())),
            persistence: Arc::new(Mutex::new(())),
            ban_gate: shared.ban_gate,
            playlist_store,
            content_store: shared.content_store,
            playlists_path: Arc::new(playlists_path),
            content_path: Arc::new(metadata_path),
        }
    }

    /// Expose the pause Arc so it can be shared with `Runtime::run`.
    pub fn pause_arc(&self) -> Arc<Mutex<PauseState>> {
        Arc::clone(&self.paused)
    }

    /// Send a manual skip-to-next swap.
    pub fn skip_next(&self) -> anyhow::Result<()> {
        self.enqueue_swap(SwapRequest {
            reason: SwapReason::Manual,
            specific: None,
        })
    }

    /// Pause cycling, optionally for a fixed duration.
    pub fn pause(&self, duration: Option<Duration>) {
        let mut p = self.paused.lock();
        p.paused = true;
        p.pause_until = checked_pause_deadline(duration);
    }

    /// Resume from pause.
    pub fn resume(&self) {
        let mut p = self.paused.lock();
        p.paused = false;
        p.pause_until = None;
    }

    /// Force-apply a specific path.
    pub fn set_specific(&self, path: PathBuf) -> anyhow::Result<()> {
        if !path.is_file() {
            anyhow::bail!("wallpaper path is not a file: {}", path.display());
        }
        let hash = target_hash(&self.index, &path, None)?;
        if self.ban_gate.is_banned(&hash) {
            anyhow::bail!("wallpaper is banned: {}", path.display());
        }
        self.enqueue_swap(SwapRequest {
            reason: SwapReason::Manual,
            specific: Some(path),
        })
    }

    /// Return a JSON status blob for IPC Status.
    pub fn status(&self) -> serde_json::Value {
        let snap = self.state.lock();
        let paused = self.paused.lock().is_paused();
        let current_paths: HashMap<String, String> = snap
            .current_path
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string_lossy().into_owned()))
            .collect();
        serde_json::json!({
            "running": true,
            "paused": paused,
            "current_path": current_paths,
            "history_len": snap.history.len(),
            "swaps_total": self.metrics.swaps_total.load(Ordering::Relaxed),
            "cache_hit_ratio": self.metrics.cache_hit_ratio(),
            "index_size": self.metrics.index_size.load(Ordering::Relaxed),
        })
    }

    pub fn stats(&self) -> serde_json::Value {
        let decode_count = self.metrics.decode_ms_count.load(Ordering::Relaxed);
        let decode_sum = self.metrics.decode_ms_sum.load(Ordering::Relaxed);
        let cache_hits = self.metrics.cache_hits.load(Ordering::Relaxed);
        let cache_misses = self.metrics.cache_misses.load(Ordering::Relaxed);
        let banned = self
            .index
            .read()
            .photos
            .iter()
            .filter(|photo| photo.banned)
            .count();
        let (playlist_count, active_playlist) = {
            let store = self.playlist_store.lock();
            (store.playlists.len(), store.active.clone())
        };

        serde_json::json!({
            "swaps_total": self.metrics.swaps_total.load(Ordering::Relaxed),
            "history_len": self.state.lock().history.len(),
            "index": {
                "photos": self.metrics.index_size.load(Ordering::Relaxed),
                "banned": banned,
            },
            "decode": {
                "count": decode_count,
                "average_ms": (decode_count > 0).then(|| decode_sum as f64 / decode_count as f64),
            },
            "cache": {
                "hits": cache_hits,
                "misses": cache_misses,
                "hit_ratio": self.metrics.cache_hit_ratio(),
            },
            "playlists": {
                "count": playlist_count,
                "active": active_playlist,
            },
        })
    }

    /// Persist and apply a photo hash ban.
    pub fn ban(&self, hash: &str) -> anyhow::Result<()> {
        let hash = normalize_ban_hash(hash)?;
        let _update = self.ban_gate.0.updates.lock();

        let path = bans_path(self.config_path.as_ref());
        let mut bans = load_bans(&path)?;
        if bans.insert(hash.clone()) {
            persist_bans(&path, &bans)?;
        }

        // The write guard waits for any already-committing apply. Once this
        // method returns, no later apply can pass the corresponding read gate.
        let mut active_bans = self.ban_gate.0.hashes.write();
        let mut index = self.index.write();
        active_bans.insert(hash.clone());
        index.ban(&hash);
        Ok(())
    }

    /// Return the last successfully applied wallpaper snapshot for each monitor.
    pub fn current_wallpaper(&self) -> HashMap<String, PathBuf> {
        self.state.lock().current_path.clone()
    }

    /// Re-read the config, playlists, and content metadata from disk, then
    /// re-scan photo sources and atomically publish the refreshed state.
    ///
    /// Schedule, transition, monitor, cache, metrics, and log-level changes
    /// require a full daemon restart.
    pub fn reload_from_disk(&self) -> anyhow::Result<()> {
        let _source_update = self.source_updates.lock();
        let src = std::fs::read_to_string(self.config_path.as_ref())
            .with_context(|| format!("read config {}", self.config_path.display()))?;
        let config = crate::config::parse::parse_kdl_config(&src)
            .with_context(|| format!("parse config {}", self.config_path.display()))?;
        let _com = ComApartment::initialize().context("initialize COM for source reload")?;
        let new_roots: Vec<PathBuf> = config
            .sources
            .iter()
            .map(|source| source.path.clone())
            .collect();

        let mut new_index = if config.sources.is_empty() {
            PhotoIndex::default()
        } else {
            let _com = ComApartment::initialize()?;
            PhotoIndex::scan_sources_cached(
                &config.sources,
                &index_cache_path(self.config_path.as_ref()),
            )
            .context("scanning photo sources during reload")?
        };

        let _persistence = self.persistence.lock();
        recover_playlist_content_transaction(
            self.playlists_path.as_ref(),
            self.content_path.as_ref(),
        )
        .context("recover playlist/content transaction before reload")?;
        let _ban_update = self.ban_gate.0.updates.lock();
        let bans = load_bans(&bans_path(self.config_path.as_ref()))?;
        new_index.apply_bans(&bans);

        let mut active_bans = self.ban_gate.0.hashes.write();
        let mut index = self.index.write();
        let mut roots = self.source_roots.write();
        let mut playlists = self.playlist_store.lock();
        let mut content = self.content_store.lock();

        let reloaded_playlists = load_playlists(self.playlists_path.as_ref())
            .with_context(|| format!("reload playlists {}", self.playlists_path.display()))?;
        let mut reloaded_content = load_content(self.content_path.as_ref())
            .with_context(|| format!("reload content metadata {}", self.content_path.display()))?;
        validate_playlist_content_consistency(&reloaded_playlists, &reloaded_content)
            .context("validate reloaded playlist/content metadata")?;
        if migrate_legacy_content(
            &mut reloaded_content,
            &reloaded_playlists,
            &new_index,
            &new_roots,
        )? {
            persist_content(&reloaded_content, self.content_path.as_ref()).with_context(|| {
                format!(
                    "persist migrated content metadata {}",
                    self.content_path.display()
                )
            })?;
        }

        let new_size = new_index.len() as u64;
        *index = new_index;
        *roots = new_roots;
        *playlists = reloaded_playlists;
        *content = reloaded_content;
        *active_bans = bans;
        self.metrics.set_index_size(new_size);
        info!(
            "reload_from_disk: photo index rebuilt with {} photos; playlists and metadata refreshed",
            new_size
        );
        Ok(())
    }

    /// Narrow the active photo pool to a single folder for this session.
    /// Pass an empty path to revert to the full configured source list.
    pub fn set_folder(&self, path: PathBuf) -> anyhow::Result<()> {
        if path.as_os_str().is_empty() {
            info!("set_folder: empty path - rebuilding configured sources");
            return self.reload_from_disk();
        }
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("set_folder: read metadata for {}", path.display()))?;
        if !metadata.is_dir() {
            anyhow::bail!("set_folder: not a directory: {}", path.display());
        }
        std::fs::read_dir(&path)
            .with_context(|| format!("set_folder: read directory {}", path.display()))?;

        let _source_update = self.source_updates.lock();
        let _com = ComApartment::initialize().context("initialize COM for folder scan")?;
        let extensions: Vec<String> = DEFAULT_IMAGE_EXTENSIONS
            .iter()
            .map(|extension| (*extension).to_string())
            .collect();

        let mut new_index = {
            let _com = ComApartment::initialize()?;
            PhotoIndex::scan(std::slice::from_ref(&path), &extensions, true)
                .with_context(|| format!("set_folder: scan {:?}", path))?
        };

        let _ban_update = self.ban_gate.0.updates.lock();
        let bans = load_bans(&bans_path(self.config_path.as_ref()))?;
        new_index.apply_bans(&bans);

        let mut active_bans = self.ban_gate.0.hashes.write();
        let new_size = self.replace_sources(new_index, vec![path.clone()]);
        *active_bans = bans;
        drop(active_bans);
        info!(
            "set_folder: index now contains {} photos from {:?}",
            new_size, path
        );
        Ok(())
    }

    fn replace_sources(&self, new_index: PhotoIndex, new_roots: Vec<PathBuf>) -> u64 {
        let new_size = new_index.len() as u64;
        let mut index = self.index.write();
        let mut roots = self.source_roots.write();
        *index = new_index;
        *roots = new_roots;
        self.metrics.set_index_size(new_size);
        new_size
    }

    // -----------------------------------------------------------------------
    // Playlist methods
    // -----------------------------------------------------------------------

    /// Return a JSON summary of all playlists + the active one.
    pub fn playlist_list(&self) -> serde_json::Value {
        let index = self.index.read();
        let store = self.playlist_store.lock();
        let content = self.content_store.lock();
        let playlists: Vec<serde_json::Value> = store
            .playlists
            .iter()
            .map(|playlist| {
                let filters = content.playlist_filters(&playlist.name);
                let dynamic = content.is_dynamic_playlist(&playlist.name);
                let path_count = if dynamic {
                    dynamic_playlist_entries(&index, &content, filters).count()
                } else {
                    playlist.paths.len()
                };
                playlist_summary_json(
                    playlist,
                    store.active.as_deref(),
                    filters,
                    dynamic,
                    path_count,
                )
            })
            .collect();
        serde_json::json!({ "playlists": playlists, "active": store.active })
    }

    /// Return one bounded page of a playlist and its per-path metadata.
    pub fn playlist_show(
        &self,
        name: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<serde_json::Value> {
        if !(1..=MAX_PLAYLIST_SHOW_LIMIT).contains(&limit) {
            anyhow::bail!(
                "playlist show limit must be between 1 and {MAX_PLAYLIST_SHOW_LIMIT}, got {limit}"
            );
        }

        let index_guard = self.index.read();
        let source_roots = self.source_roots.read();
        let store = self.playlist_store.lock();
        let content = self.content_store.lock();
        let playlist = store
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        let filters = content.playlist_filters(&playlist.name);
        let dynamic = content.is_dynamic_playlist(&playlist.name);
        if dynamic {
            let entries: Vec<&PhotoEntry> =
                dynamic_playlist_entries(&index_guard, &content, filters).collect();
            let total = entries.len();
            let summary =
                playlist_summary_json(playlist, store.active.as_deref(), filters, true, total);
            return bounded_playlist_show(&summary, total, offset, limit, |index| {
                let entry = entries[index];
                Ok(dynamic_playlist_item_json(entry, content.get(&entry.hash)))
            });
        }

        let total = playlist.paths.len();
        let summary =
            playlist_summary_json(playlist, store.active.as_deref(), filters, false, total);
        bounded_playlist_show(&summary, total, offset, limit, |index| {
            let path = &playlist.paths[index];
            let identity = resolve_content(&index_guard, &content, path, &source_roots, false)?;
            let metadata = identity
                .as_ref()
                .and_then(|identity| content.get(&identity.hash));
            Ok(playlist_item_json(
                playlist,
                path,
                identity.as_ref(),
                metadata,
                content.is_legacy_pending(&playlist.name, path),
            ))
        })
    }

    /// Create an empty static or dynamic playlist and persist both stores.
    pub fn playlist_create(&self, name: &str, dynamic: bool) -> anyhow::Result<()> {
        let _persistence = self.persistence.lock();
        self.recover_pending_transaction()?;
        let mut current_playlists = self.playlist_store.lock();
        let mut current_content = self.content_store.lock();
        let mut next_playlists = current_playlists.clone();
        let mut next_content = current_content.clone();
        next_playlists.create(name)?;
        next_content.set_dynamic_playlist(name, dynamic)?;
        self.persist_playlist_and_content(&next_content, &next_playlists)?;
        *current_playlists = next_playlists;
        *current_content = next_content;
        Ok(())
    }

    /// Add a path to a playlist and persist.
    pub fn playlist_add(&self, name: &str, path: &str) -> anyhow::Result<()> {
        self.update_playlist_entry(name, path, |store, path| store.add_path(name, path))
    }

    pub fn playlist_tag(
        &self,
        name: &str,
        path: &str,
        kind: &str,
        tags: Vec<String>,
    ) -> anyhow::Result<()> {
        match self.resolve_playlist_content(name, path, true) {
            Ok((_, identity)) => self.update_content(|content| {
                content.set_tag_group(
                    &identity.hash,
                    &identity.aliases,
                    kind,
                    tags,
                    (identity.width, identity.height),
                )
            }),
            Err(identity_error) => {
                debug!("using legacy path metadata because content identity failed: {identity_error:#}");
                self.update_playlist_entry(name, path, |store, path| {
                    store.set_tag_group(name, path, kind, tags)
                })
            }
        }
    }

    pub fn playlist_rate(&self, name: &str, path: &str, rating: u8) -> anyhow::Result<()> {
        match self.resolve_playlist_content(name, path, true) {
            Ok((_, identity)) => self.update_content(|content| {
                content.set_rating(
                    &identity.hash,
                    &identity.aliases,
                    rating,
                    (identity.width, identity.height),
                )
            }),
            Err(identity_error) => {
                debug!(
                    "using legacy path rating because content identity failed: {identity_error:#}"
                );
                self.update_playlist_entry(name, path, |store, path| {
                    store.set_rating(name, path, rating)
                })
            }
        }
    }

    pub fn playlist_frequency(&self, name: &str, path: &str, frequency: u32) -> anyhow::Result<()> {
        self.update_playlist_entry(name, path, |store, path| {
            store.set_frequency(name, path, frequency)
        })
    }

    pub fn playlist_shuffle(&self, name: &str, shuffle: bool) -> anyhow::Result<()> {
        self.update_playlists(|store| store.set_shuffle(name, shuffle))
    }

    pub fn playlist_filter(
        &self,
        name: &str,
        include: BTreeMap<String, Vec<String>>,
        exclude: BTreeMap<String, Vec<String>>,
    ) -> anyhow::Result<()> {
        let _persistence = self.persistence.lock();
        self.recover_pending_transaction()?;
        let playlists = self.playlist_store.lock();
        if playlists.get(name).is_none() {
            anyhow::bail!("playlist '{}' not found", name);
        }
        let mut current = self.content_store.lock();
        let mut next = current.clone();
        next.set_playlist_filters(name, include, exclude)?;
        persist_content(&next, self.content_path.as_ref())?;
        *current = next;
        Ok(())
    }

    /// Return whether one path already has tags or a rating without serializing
    /// the complete playlist store.
    pub fn playlist_autotag_status(&self, name: &str, path: &str) -> anyhow::Result<bool> {
        validate_autotag_target(name, path)?;
        let index = self.index.read();
        let source_roots = self.source_roots.read();
        let store = self.playlist_store.lock();
        let path = resolve_playlist_entry_path(&store, name, path, &source_roots)?;
        let content = self.content_store.lock();
        if content.is_dynamic_playlist(name) {
            anyhow::bail!(
                "playlist '{name}' is dynamic; edit shared metadata with `aurora-ctl content` instead"
            );
        }
        let local = playlist_path_has_autotag_metadata(&store, name, &path);
        let is_member = store
            .get(name)
            .is_some_and(|playlist| playlist.paths.iter().any(|stored| stored == &path));
        if !is_member {
            return Ok(false);
        }
        let global = resolve_content(&index, &content, &path, &source_roots, false)?
            .is_some_and(|identity| content.has_autotag_metadata(&identity.hash));
        Ok(local || global)
    }

    /// Add one path and apply all supplied autotag metadata in one persisted
    /// playlist transaction. Returns false when an existing tagged path wins.
    #[allow(clippy::too_many_arguments)]
    pub fn playlist_autotag_upsert(
        &self,
        name: &str,
        path: &str,
        mut groups: BTreeMap<String, Vec<String>>,
        rating: Option<u8>,
        frequency: Option<u32>,
        provenance: Option<AutoTagProvenance>,
        create_playlist: bool,
        overwrite_existing: bool,
    ) -> anyhow::Result<bool> {
        validate_autotag_target(name, path)?;
        if rating.is_some_and(|rating| rating > 5) {
            anyhow::bail!("autotag rating must be between 0 and 5");
        }
        if frequency == Some(0) {
            anyhow::bail!("autotag frequency must be at least 1");
        }
        if let Some(provenance) = &provenance {
            provenance.validate()?;
        }
        let has_provenance = provenance.is_some();
        if groups.keys().any(|kind| kind.trim().is_empty()) {
            anyhow::bail!("autotag tag kind must not be empty");
        }
        groups.retain(|_, tags| tags.iter().any(|tag| !tag.trim().is_empty()));
        if groups.is_empty() && rating.is_none() && frequency.is_none() && !has_provenance {
            anyhow::bail!(
                "autotag update contains no tags, rating, or frequency and no provenance"
            );
        }

        let _persistence = self.persistence.lock();
        self.recover_pending_transaction()?;
        // Keep the existing index -> roots -> playlists -> content lock order.
        let index = self.index.read();
        let source_roots = self.source_roots.read();
        let mut current_playlists = self.playlist_store.lock();
        if current_playlists.get(name).is_none() && !create_playlist {
            anyhow::bail!("playlist '{}' not found", name);
        }
        let stored = resolve_playlist_entry_path(&current_playlists, name, path, &source_roots)?;
        let mut current_content = self.content_store.lock();
        if current_content.is_dynamic_playlist(name) {
            anyhow::bail!(
                "playlist '{name}' is dynamic; edit shared metadata with `aurora-ctl content` instead"
            );
        }
        let is_member = current_playlists
            .get(name)
            .is_some_and(|playlist| playlist.paths.iter().any(|path| path == &stored));
        let local =
            is_member && playlist_path_has_autotag_metadata(&current_playlists, name, &stored);
        let identity = match resolve_content(&index, &current_content, &stored, &source_roots, true)
        {
            Ok(identity) => identity,
            Err(error) => {
                debug!("using legacy autotag metadata because content identity failed: {error:#}");
                None
            }
        };
        let global = is_member
            && identity
                .as_ref()
                .is_some_and(|identity| current_content.has_autotag_metadata(&identity.hash));
        if is_member && !overwrite_existing && (local || global) {
            return Ok(false);
        }
        if identity.is_none() && groups.is_empty() && rating.is_none() && frequency.is_none() {
            anyhow::bail!("cannot store autotag provenance without an identifiable image");
        }

        let mut next_content = current_content.clone();
        if let Some(identity) = &identity {
            if overwrite_existing {
                next_content.clear_metadata(&identity.hash)?;
            }
            for (kind, tags) in &groups {
                next_content.set_tag_group(
                    &identity.hash,
                    &identity.aliases,
                    kind,
                    tags.clone(),
                    (identity.width, identity.height),
                )?;
            }
            if let Some(rating) = rating {
                next_content.set_rating(
                    &identity.hash,
                    &identity.aliases,
                    rating,
                    (identity.width, identity.height),
                )?;
            }
            if let Some(provenance) = provenance {
                next_content.set_autotag(
                    &identity.hash,
                    &identity.aliases,
                    provenance,
                    (identity.width, identity.height),
                )?;
            }
        } else if has_provenance {
            debug!("autotag provenance omitted because content identity is unavailable");
        }

        let mut next_playlists = current_playlists.clone();
        if next_playlists.get(name).is_none() {
            next_playlists.create(name)?;
        }
        if !next_playlists
            .get(name)
            .expect("playlist was checked or created")
            .paths
            .iter()
            .any(|path| path == &stored)
        {
            next_playlists.add_path(name, &stored)?;
        }
        if overwrite_existing {
            next_playlists.clear_path_metadata(name, &stored)?;
        }
        for (kind, tags) in groups {
            next_playlists.set_tag_group(name, &stored, &kind, tags)?;
        }
        if let Some(rating) = rating {
            next_playlists.set_rating(name, &stored, rating)?;
        }
        if let Some(frequency) = frequency {
            next_playlists.set_frequency(name, &stored, frequency)?;
        }

        self.persist_playlist_and_content(&next_content, &next_playlists)?;

        *current_playlists = next_playlists;
        *current_content = next_content;
        Ok(true)
    }

    /// Remove a path from a playlist and persist.
    pub fn playlist_remove(&self, name: &str, path: &str) -> anyhow::Result<()> {
        self.update_playlist_entry(name, path, |store, path| store.remove_path(name, path))
    }

    /// Activate and persist a playlist, then request an immediate swap best-effort.
    pub fn playlist_activate(&self, name: &str) -> anyhow::Result<()> {
        self.update_playlists(|store| store.activate(name))?;
        if let Err(error) = self.enqueue_swap(SwapRequest {
            reason: SwapReason::Manual,
            specific: None,
        }) {
            warn!("playlist '{name}' activated, but its immediate swap was not queued: {error}");
        }
        Ok(())
    }

    /// Deactivate the current playlist and persist.
    pub fn playlist_deactivate(&self) -> anyhow::Result<()> {
        self.update_playlists(|store| {
            store.deactivate();
            Ok(())
        })
    }

    /// Delete a playlist and persist.
    pub fn playlist_delete(&self, name: &str) -> anyhow::Result<()> {
        let _persistence = self.persistence.lock();
        self.recover_pending_transaction()?;
        let mut current_playlists = self.playlist_store.lock();
        let mut current_content = self.content_store.lock();
        let mut next_playlists = current_playlists.clone();
        let mut next_content = current_content.clone();
        next_playlists.delete(name)?;
        next_content.remove_playlist_filters(name);
        next_content.remove_pending_playlist(name);
        next_content.remove_dynamic_playlist(name);
        self.persist_playlist_and_content(&next_content, &next_playlists)?;
        *current_playlists = next_playlists;
        *current_content = next_content;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Shared content metadata methods
    // -----------------------------------------------------------------------

    pub fn content_list(
        &self,
        offset: usize,
        limit: usize,
        include: BTreeMap<String, Vec<String>>,
        exclude: BTreeMap<String, Vec<String>>,
    ) -> anyhow::Result<serde_json::Value> {
        if !(1..=MAX_CONTENT_LIST_LIMIT).contains(&limit) {
            anyhow::bail!(
                "content list limit must be between 1 and {MAX_CONTENT_LIST_LIMIT}, got {limit}"
            );
        }
        let filters = TagFilters::new(include, exclude)?;
        let index = self.index.read();
        let content = self.content_store.lock();
        let total = content
            .iter()
            .filter(|(_, metadata)| filters.accepts(Some(&metadata.tag_groups)))
            .count();
        let requested: Vec<(&str, &ContentMetadata)> = content
            .iter()
            .filter(|(_, metadata)| filters.accepts(Some(&metadata.tag_groups)))
            .skip(offset)
            .take(limit)
            .collect();
        bounded_content_list(total, offset, limit, |index_in_store| {
            let (hash, metadata) = requested[index_in_store - offset];
            Ok(content_item_json(&index, hash, Some(metadata), None, false))
        })
    }

    pub fn content_show(&self, target: &str) -> anyhow::Result<serde_json::Value> {
        let target = self.resolve_current_content_target(target)?;
        let index = self.index.read();
        let source_roots = self.source_roots.read();
        let content = self.content_store.lock();
        let identity = resolve_content_target(&index, &content, &target, &source_roots)?;
        Ok(content_item_json(
            &index,
            &identity.hash,
            content.get(&identity.hash),
            Some(&identity),
            true,
        ))
    }

    pub fn content_tag(&self, target: &str, kind: &str, tags: Vec<String>) -> anyhow::Result<()> {
        self.update_content_target(target, |content, identity| {
            content.set_tag_group(
                &identity.hash,
                &identity.aliases,
                kind,
                tags,
                (identity.width, identity.height),
            )
        })
    }

    pub fn content_rate(&self, target: &str, rating: u8) -> anyhow::Result<()> {
        self.update_content_target(target, |content, identity| {
            content.set_rating(
                &identity.hash,
                &identity.aliases,
                rating,
                (identity.width, identity.height),
            )
        })
    }

    pub fn content_clear(&self, target: &str) -> anyhow::Result<()> {
        self.update_content_target(target, |content, identity| {
            content.clear_metadata(&identity.hash)
        })
    }

    fn resolve_current_content_target(&self, target: &str) -> anyhow::Result<String> {
        if !target.trim().eq_ignore_ascii_case("current") {
            return Ok(target.to_string());
        }
        let paths: BTreeSet<PathBuf> = self
            .state
            .lock()
            .current_path
            .values()
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
            .collect();
        match paths.len() {
            0 => anyhow::bail!("no current wallpaper reported by the runtime"),
            1 => Ok(paths
                .into_iter()
                .next()
                .expect("one current wallpaper")
                .display()
                .to_string()),
            _ => anyhow::bail!(
                "monitors have different current wallpapers; pass an explicit path instead of 'current'"
            ),
        }
    }

    fn update_content_target(
        &self,
        target: &str,
        mutation: impl FnOnce(&mut ContentStore, &ResolvedContent) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let target = self.resolve_current_content_target(target)?;
        let _persistence = self.persistence.lock();
        self.recover_pending_transaction()?;
        let index = self.index.read();
        let source_roots = self.source_roots.read();
        let mut current = self.content_store.lock();
        let identity = resolve_content_target(&index, &current, &target, &source_roots)?;
        let mut next = current.clone();
        mutation(&mut next, &identity)?;
        persist_content(&next, self.content_path.as_ref())?;
        *current = next;
        Ok(())
    }

    fn resolve_playlist_content(
        &self,
        name: &str,
        path: &str,
        hash_unindexed: bool,
    ) -> anyhow::Result<(String, ResolvedContent)> {
        let index = self.index.read();
        let source_roots = self.source_roots.read();
        let store = self.playlist_store.lock();
        let stored = resolve_playlist_entry_path(&store, name, path, &source_roots)?;
        let playlist = store
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("playlist '{}' not found", name))?;
        let content = self.content_store.lock();
        if content.is_dynamic_playlist(name) {
            anyhow::bail!(
                "playlist '{name}' is dynamic; edit shared metadata with `aurora-ctl content` instead"
            );
        }
        if !playlist.paths.iter().any(|path| path == &stored) {
            anyhow::bail!("path '{}' not in playlist '{}'", path, name);
        }
        let identity = resolve_content(&index, &content, &stored, &source_roots, hash_unindexed)?
            .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot identify image content for path '{}' in playlist '{}'",
                path,
                name
            )
        })?;
        Ok((stored, identity))
    }

    fn update_content<T>(
        &self,
        mutation: impl FnOnce(&mut ContentStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _persistence = self.persistence.lock();
        self.recover_pending_transaction()?;
        let mut current = self.content_store.lock();
        let mut next = current.clone();
        let result = mutation(&mut next)?;
        persist_content(&next, self.content_path.as_ref())?;
        *current = next;
        Ok(result)
    }

    fn persist_playlist_and_content(
        &self,
        next_content: &ContentStore,
        next_playlists: &PlaylistStore,
    ) -> anyhow::Result<()> {
        commit_playlist_content_transaction(
            next_playlists,
            next_content,
            self.playlists_path.as_ref(),
            self.content_path.as_ref(),
        )
    }

    fn update_playlists<T>(
        &self,
        mutation: impl FnOnce(&mut PlaylistStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _persistence = self.persistence.lock();
        self.recover_pending_transaction()?;
        let mut current = self.playlist_store.lock();
        let mut next = current.clone();
        let result = mutation(&mut next)?;
        persist_playlists(&next, self.playlists_path.as_ref())?;
        *current = next;
        Ok(result)
    }

    fn recover_pending_transaction(&self) -> anyhow::Result<()> {
        recover_playlist_content_transaction(
            self.playlists_path.as_ref(),
            self.content_path.as_ref(),
        )
    }

    fn update_playlist_entry<T>(
        &self,
        name: &str,
        path: &str,
        mutation: impl FnOnce(&mut PlaylistStore, &str) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _persistence = self.persistence.lock();
        self.recover_pending_transaction()?;
        let source_roots = self.source_roots.read();
        let mut current = self.playlist_store.lock();
        let content = self.content_store.lock();
        if content.is_dynamic_playlist(name) {
            anyhow::bail!(
                "playlist '{name}' is dynamic; edit shared metadata with `aurora-ctl content` instead"
            );
        }
        let path = resolve_playlist_entry_path(&current, name, path, &source_roots)?;
        let mut next = current.clone();
        let result = mutation(&mut next, &path)?;
        persist_playlists(&next, self.playlists_path.as_ref())?;
        *current = next;
        Ok(result)
    }

    // -----------------------------------------------------------------------

    /// Restore the previous photo from the history ring.
    /// Returns an error if there is no previous entry (history has fewer than 2 entries).
    pub fn prev(&self) -> anyhow::Result<()> {
        {
            let snap = self.state.lock();
            if snap.history.len() < 2 {
                return Err(anyhow::anyhow!("no previous photo in history"));
            }
        }
        self.enqueue_swap(SwapRequest {
            reason: SwapReason::Previous,
            specific: None,
        })
    }

    fn enqueue_swap(&self, request: SwapRequest) -> anyhow::Result<()> {
        self.swap_tx.try_send(request).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => anyhow::anyhow!("runtime swap queue is busy"),
            mpsc::error::TrySendError::Closed(_) => {
                anyhow::anyhow!("runtime swap channel is closed")
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn com_apartment_balances_on_a_fresh_thread() {
        std::thread::spawn(|| {
            for _ in 0..2 {
                let guard = ComApartment::initialize().unwrap();
                drop(guard);
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn partial_monitor_success_commits_only_successful_effects() {
        let mut state = RuntimeState::new();
        let metrics = Metrics::new();
        let (event_tx, mut events) = tokio::sync::broadcast::channel(4);
        let path = PathBuf::from("wallpaper.jpg");
        let successful = vec!["DISPLAY1".to_string()];

        commit_successful_monitors(
            &mut state,
            &metrics,
            Some(&event_tx),
            &path,
            &SwapReason::Manual,
            5,
            &successful,
        );
        let failures = vec!["monitor DISPLAY2: access denied".to_string()];
        monitor_results(1, &failures).unwrap();

        assert_eq!(
            state.current_path,
            HashMap::from([("DISPLAY1".to_string(), path.clone())])
        );
        assert_eq!(
            *metrics.current_photo.lock(),
            HashMap::from([("DISPLAY1".to_string(), path.clone())])
        );
        assert_eq!(metrics.swaps_total.load(Ordering::Relaxed), 1);
        assert_eq!(state.history, VecDeque::from([path.clone()]));
        assert_eq!(state.recent_paths, VecDeque::from([path.clone()]));

        match events.try_recv().unwrap() {
            IpcEvent::WallpaperChanged {
                monitor_id,
                path: event_path,
            } => {
                assert_eq!(monitor_id, "DISPLAY1");
                assert_eq!(event_path, path.display().to_string());
            }
            event => panic!("unexpected first event: {event:?}"),
        }
        match events.try_recv().unwrap() {
            IpcEvent::Swapped {
                monitor,
                path: event_path,
                ..
            } => {
                assert_eq!(monitor, "DISPLAY1");
                assert_eq!(event_path, path.display().to_string());
            }
            event => panic!("unexpected second event: {event:?}"),
        }
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn direct_apply_replaces_stale_per_monitor_state_with_all() {
        let old_path = PathBuf::from("old.jpg");
        let new_path = PathBuf::from("new.jpg");
        let mut state = RuntimeState::new();
        state
            .current_path
            .insert("DISPLAY1".to_string(), old_path.clone());
        let metrics = Metrics::new();
        metrics.set_current_photo("DISPLAY1", old_path);

        commit_successful_monitors(
            &mut state,
            &metrics,
            None,
            &new_path,
            &SwapReason::Interval,
            5,
            &[ALL_MONITORS_ID.to_string()],
        );

        let expected = HashMap::from([(ALL_MONITORS_ID.to_string(), new_path)]);
        assert_eq!(state.current_path, expected);
        assert_eq!(*metrics.current_photo.lock(), expected);
    }

    #[test]
    fn automatic_swap_suppresses_targets_already_active_everywhere() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("wallpaper.jpg");
        std::fs::write(&target, b"wallpaper").unwrap();
        let alias = directory.path().join(".").join("wallpaper.jpg");
        let current = HashMap::from([
            ("DISPLAY1".to_string(), target.clone()),
            ("DISPLAY2".to_string(), alias),
        ]);
        let attached = vec!["DISPLAY1".to_string(), "DISPLAY2".to_string()];

        assert!(should_suppress_same_target(
            &SwapReason::Interval,
            &current,
            &target,
            &attached,
        ));
        assert!(should_suppress_same_target(
            &SwapReason::AtTime,
            &current,
            &target,
            &attached,
        ));
        assert!(!should_suppress_same_target(
            &SwapReason::Manual,
            &current,
            &target,
            &attached,
        ));
        assert!(!should_suppress_same_target(
            &SwapReason::Previous,
            &current,
            &target,
            &attached,
        ));
        assert!(!should_suppress_same_target(
            &SwapReason::Interval,
            &HashMap::new(),
            &target,
            &attached,
        ));
        assert!(!should_suppress_same_target(
            &SwapReason::Interval,
            &current,
            &target,
            &["DISPLAY1".to_string(), "DISPLAY3".to_string()],
        ));
    }

    #[test]
    fn startup_wallpapers_seed_the_anti_repeat_window() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("current.jpg");
        std::fs::write(&current, b"wallpaper").unwrap();
        let alias = directory.path().join(".").join("current.jpg");
        let mut state = RuntimeState::new();
        state
            .current_path
            .insert("DISPLAY1".to_string(), current.clone());
        state.current_path.insert("DISPLAY2".to_string(), alias);

        seed_recent_current_paths(&mut state);

        assert_eq!(
            state.recent_paths,
            VecDeque::from([current.canonicalize().unwrap()])
        );
    }

    #[test]
    fn startup_cadence_requires_wallpaper_coverage_for_every_monitor() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("current.jpg");
        std::fs::write(&current, b"wallpaper").unwrap();
        let monitor = |id: &str| crate::apply::MonitorInfo {
            id: id.to_string(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let (partial, complete) = runtime_state_from_snapshots(vec![
            MonitorSnapshot {
                monitor: monitor("DISPLAY1"),
                current_path: Some(current.clone()),
            },
            MonitorSnapshot {
                monitor: monitor("DISPLAY2"),
                current_path: None,
            },
        ]);
        assert!(!complete);
        assert_eq!(partial.current_path.len(), 1);

        let (covered, complete) = runtime_state_from_snapshots(vec![MonitorSnapshot {
            monitor: monitor("DISPLAY1"),
            current_path: Some(current),
        }]);
        assert!(complete);
        assert_eq!(covered.current_path.len(), 1);
    }

    #[test]
    fn detached_monitors_are_pruned_without_dropping_attached_failures() {
        let mut state = RuntimeState::new();
        state
            .current_path
            .insert("attached".to_string(), PathBuf::from("old.jpg"));
        state
            .current_path
            .insert("detached".to_string(), PathBuf::from("gone.jpg"));
        let metrics = Metrics::new();
        metrics.set_current_photo("attached", PathBuf::from("old.jpg"));
        metrics.set_current_photo("detached", PathBuf::from("gone.jpg"));

        reconcile_attached_monitors(&mut state, &metrics, &["attached".to_string()]);

        assert_eq!(
            state.current_path,
            HashMap::from([("attached".to_string(), PathBuf::from("old.jpg"))])
        );
        assert_eq!(*metrics.current_photo.lock(), state.current_path);
    }

    #[test]
    fn all_monitor_fallback_expands_when_enumeration_recovers() {
        let mut state = RuntimeState::new();
        state
            .current_path
            .insert(ALL_MONITORS_ID.to_string(), PathBuf::from("current.jpg"));
        let metrics = Metrics::new();
        metrics.set_current_photo(ALL_MONITORS_ID, PathBuf::from("current.jpg"));

        reconcile_attached_monitors(
            &mut state,
            &metrics,
            &["DISPLAY1".to_string(), "DISPLAY2".to_string()],
        );

        let expected = HashMap::from([
            ("DISPLAY1".to_string(), PathBuf::from("current.jpg")),
            ("DISPLAY2".to_string(), PathBuf::from("current.jpg")),
        ]);
        assert_eq!(state.current_path, expected);
        assert_eq!(*metrics.current_photo.lock(), expected);
    }

    #[test]
    fn zero_monitor_success_commits_no_effects() {
        let mut state = RuntimeState::new();
        let metrics = Metrics::new();
        let (event_tx, mut events) = tokio::sync::broadcast::channel(2);

        commit_successful_monitors(
            &mut state,
            &metrics,
            Some(&event_tx),
            Path::new("wallpaper.jpg"),
            &SwapReason::Manual,
            5,
            &[],
        );
        let error = monitor_results(0, &["monitor DISPLAY2: access denied".to_string()])
            .unwrap_err()
            .to_string();

        assert!(error.contains("monitor DISPLAY2: access denied"));
        assert!(state.current_path.is_empty());
        assert!(metrics.current_photo.lock().is_empty());
        assert_eq!(metrics.swaps_total.load(Ordering::Relaxed), 0);
        assert!(state.history.is_empty());
        assert!(state.recent_paths.is_empty());
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn active_ineligible_playlist_blocks_global_runtime_selection() {
        let outside = PathBuf::from("outside-indexed-wallpaper.jpg");
        let mut index = PhotoIndex::default();
        index.photos.push(crate::index::PhotoEntry {
            path: outside.clone(),
            width: None,
            height: None,
            hash: "outside-hash".to_string(),
            banned: false,
        });

        let recent = VecDeque::new();
        assert_eq!(
            rotation_target(&index, false, None, 0, &recent).unwrap().0,
            outside
        );

        let mut playlists = PlaylistStore::default();
        playlists.create("isolated").unwrap();
        playlists
            .add_path("isolated", "missing-playlist-wallpaper.jpg")
            .unwrap();
        playlists.activate("isolated").unwrap();
        let playlist_pick =
            playlists.pick_from_roots(&[], &mut HashMap::new(), 0, &recent, &HashSet::new());
        assert!(playlist_pick.is_none());

        let error = rotation_target(
            &index,
            playlists.active.is_some(),
            playlist_pick.map(|path| (path, "playlist-hash".to_string())),
            0,
            &recent,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("active playlist has no eligible"));
        assert!(error.contains("aurora-ctl playlist deactivate"));
    }

    /// Convenience wrapper for tests that don't care about playlist persistence.
    fn make_handle(
        tx: mpsc::Sender<SwapRequest>,
        state: Arc<Mutex<RuntimeStateSnapshot>>,
        index: Arc<RwLock<PhotoIndex>>,
        metrics: Arc<Metrics>,
    ) -> RuntimeHandle {
        let bans = index
            .read()
            .photos
            .iter()
            .filter(|entry| entry.banned)
            .map(|entry| entry.hash.clone())
            .collect();
        RuntimeHandle::new(
            tx,
            state,
            RuntimeShared::new(
                index,
                Arc::new(RwLock::new(Vec::new())),
                BanGate::new(bans),
                Arc::new(Mutex::new(ContentStore::default())),
            ),
            metrics,
            std::path::PathBuf::from("config.kdl"),
            Arc::new(Mutex::new(PlaylistStore::default())),
            std::path::PathBuf::from("playlists.kdl"),
        )
    }

    fn make_playlist_handle(
        playlists_path: PathBuf,
        store: Arc<Mutex<PlaylistStore>>,
    ) -> RuntimeHandle {
        let (tx, _rx) = mpsc::channel(4);
        RuntimeHandle::new(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            RuntimeShared::new(
                Arc::new(RwLock::new(PhotoIndex::default())),
                Arc::new(RwLock::new(Vec::new())),
                BanGate::default(),
                Arc::new(Mutex::new(ContentStore::default())),
            ),
            Metrics::new(),
            playlists_path.with_file_name("config.kdl"),
            store,
            playlists_path,
        )
    }

    fn make_source_handle(
        config_path: PathBuf,
        index: Arc<RwLock<PhotoIndex>>,
        source_roots: Arc<RwLock<Vec<PathBuf>>>,
        metrics: Arc<Metrics>,
    ) -> RuntimeHandle {
        let (tx, _rx) = mpsc::channel(4);
        let bans = index
            .read()
            .photos
            .iter()
            .filter(|entry| entry.banned)
            .map(|entry| entry.hash.clone())
            .collect();
        RuntimeHandle::new(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            RuntimeShared::new(
                index,
                source_roots,
                BanGate::new(bans),
                Arc::new(Mutex::new(ContentStore::default())),
            ),
            metrics,
            config_path,
            Arc::new(Mutex::new(PlaylistStore::default())),
            PathBuf::from("playlists.kdl"),
        )
    }

    fn write_test_bmp(path: &Path, color: [u8; 3]) {
        use image::{ImageBuffer, Rgb};

        let image: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(16, 16, Rgb(color));
        image.save(path).unwrap();
    }

    fn source_config(path: &Path) -> String {
        format!(
            "source {{\n    path \"{}\"\n    recursive true\n    extensions \"bmp\"\n    min-width 0\n    min-height 0\n}}\n",
            path.display().to_string().replace('\\', "/")
        )
    }

    #[test]
    fn dynamic_playlist_selects_and_pages_current_index_matches() {
        let directory = tempfile::tempdir().unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let mut playlists = PlaylistStore::default();
        playlists.create("focus").unwrap();
        playlists.activate("focus").unwrap();
        let playlists = Arc::new(Mutex::new(playlists));
        let handle = make_playlist_handle(playlists_path, playlists);

        let first_hash = "a".repeat(64);
        let ignored_hash = "b".repeat(64);
        let second_hash = "c".repeat(64);
        let banned_hash = "d".repeat(64);
        let first = PathBuf::from("first.bmp");
        let ignored = PathBuf::from("ignored.bmp");
        let second = PathBuf::from("second.bmp");
        let banned = PathBuf::from("banned.bmp");
        handle.index.write().photos = vec![
            PhotoEntry {
                path: first.clone(),
                width: Some(10),
                height: Some(11),
                hash: first_hash.clone(),
                banned: false,
            },
            PhotoEntry {
                path: ignored,
                width: Some(20),
                height: Some(21),
                hash: ignored_hash.clone(),
                banned: false,
            },
            PhotoEntry {
                path: second.clone(),
                width: Some(30),
                height: Some(31),
                hash: second_hash.clone(),
                banned: false,
            },
            PhotoEntry {
                path: banned,
                width: Some(40),
                height: Some(41),
                hash: banned_hash.clone(),
                banned: true,
            },
        ];

        {
            let mut content = handle.content_store.lock();
            content.set_dynamic_playlist("focus", true).unwrap();
            content
                .set_playlist_filters(
                    "focus",
                    BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                    BTreeMap::new(),
                )
                .unwrap();
            for hash in [&first_hash, &second_hash, &banned_hash] {
                content
                    .set_tag_group(hash, &[], "theme", vec!["night".to_string()], (None, None))
                    .unwrap();
            }
            content
                .set_tag_group(
                    &ignored_hash,
                    &[],
                    "theme",
                    vec!["day".to_string()],
                    (None, None),
                )
                .unwrap();
            content
                .set_rating(&first_hash, &[], 4, (None, None))
                .unwrap();
        }

        let candidates = {
            let index = handle.index.read();
            let content = handle.content_store.lock();
            dynamic_playlist_candidates(
                &index,
                &content,
                content.playlist_filters("focus"),
                &HashSet::new(),
            )
        };
        assert_eq!(candidates, vec![(first.clone(), 5), (second.clone(), 1)]);
        assert_eq!(
            pick_weighted_path(
                "focus",
                false,
                &candidates,
                &mut HashMap::new(),
                1,
                &VecDeque::from([first])
            ),
            Some(second.clone())
        );

        let list = handle.playlist_list();
        assert_eq!(list["playlists"][0]["dynamic"], true);
        assert_eq!(list["playlists"][0]["path_count"], 2);

        let page = handle.playlist_show("focus", 1, 1).unwrap();
        assert_eq!(page["playlist"]["dynamic"], true);
        assert_eq!(page["total"], 2);
        assert_eq!(page["items"][0]["path"], second.display().to_string());
        assert_eq!(
            page["items"][0]["content_id"],
            format!("blake3:{second_hash}")
        );
        assert_eq!(page["items"][0]["width"], 30);
        assert_eq!(page["items"][0]["frequency"], 1);

        {
            let mut content = handle.content_store.lock();
            content
                .set_playlist_filters(
                    "focus",
                    BTreeMap::from([("theme".to_string(), vec!["missing".to_string()])]),
                    BTreeMap::new(),
                )
                .unwrap();
        }
        let no_match = {
            let index = handle.index.read();
            let content = handle.content_store.lock();
            dynamic_playlist_candidates(
                &index,
                &content,
                content.playlist_filters("focus"),
                &HashSet::new(),
            )
        };
        assert!(no_match.is_empty());
        assert!(rotation_target(&handle.index.read(), true, None, 0, &VecDeque::new()).is_err());
    }

    #[test]
    fn content_commands_resolve_paths_ids_and_current_and_persist() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("shared.bmp");
        write_test_bmp(&image, [4, 5, 6]);
        let playlists_path = directory.path().join("playlists.kdl");
        let handle = make_playlist_handle(
            playlists_path,
            Arc::new(Mutex::new(PlaylistStore::default())),
        );
        *handle.index.write() = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        let hash = handle.index.read().photos[0].hash.clone();
        let content_id = format!("blake3:{hash}");

        handle
            .content_tag(&image.to_string_lossy(), "theme", vec!["night".to_string()])
            .unwrap();
        handle.content_rate(&content_id, 5).unwrap();

        let shown = handle.content_show(&content_id).unwrap();
        assert_eq!(shown["content_id"], content_id);
        assert_eq!(shown["tag_groups"]["theme"][0], "night");
        assert_eq!(shown["rating"], 5);
        assert_eq!(shown["orphaned"], false);
        assert_eq!(shown["indexed_paths"][0], image.display().to_string());

        let matching = handle
            .content_list(
                0,
                1,
                BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(matching["total"], 1);
        let missing = handle
            .content_list(
                0,
                1,
                BTreeMap::from([("theme".to_string(), vec!["day".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(missing["total"], 0);

        handle
            .state
            .lock()
            .current_path
            .insert("display".to_string(), image.clone());
        handle.content_clear("current").unwrap();
        let cleared = load_content(&directory.path().join("content.json")).unwrap();
        let metadata = cleared.get(&hash).unwrap();
        assert!(metadata.tag_groups.is_empty());
        assert_eq!(metadata.rating, None);
        assert!(!metadata.aliases.is_empty());
    }

    #[test]
    fn content_show_reports_unindexed_file_identity_without_persisting() {
        let _com = ComApartment::initialize().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("external.bmp");
        write_test_bmp(&image, [7, 8, 9]);
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(PlaylistStore::default())),
        );

        let shown = handle.content_show(&image.to_string_lossy()).unwrap();

        assert_eq!(shown["orphaned"], false);
        assert_eq!(
            shown["resolved_path"],
            image.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(shown["aliases"][0], image.display().to_string());
        assert_eq!(shown["width"], 16);
        assert_eq!(shown["height"], 16);
        assert!(shown["indexed_paths"].as_array().unwrap().is_empty());
        assert!(!directory.path().join("content.json").exists());
    }

    #[test]
    fn content_list_bounds_rendering_and_wire_size_before_returning() {
        let directory = tempfile::tempdir().unwrap();
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(PlaylistStore::default())),
        );
        for index in 0..5 {
            let hash = format!("{index:064x}");
            handle
                .content_store
                .lock()
                .set_autotag(
                    &hash,
                    &[format!(r"\\offline.invalid\wallpapers\{index}.jpg")],
                    AutoTagProvenance {
                        model: "model".to_string(),
                        raw: serde_json::json!({"blob": "x".repeat(240 * 1024)}),
                        ..Default::default()
                    },
                    (None, None),
                )
                .unwrap();
        }

        let page = handle
            .content_list(0, 5, BTreeMap::new(), BTreeMap::new())
            .unwrap();
        let items = page["items"].as_array().unwrap();
        assert_eq!(page["total"], 5);
        assert!(!items.is_empty());
        assert!(items.len() < 5);
        assert_eq!(page["next_offset"], items.len());
        assert!(playlist_show_wire_len(&page).unwrap() <= MAX_FRAME_SIZE);
        assert!(items
            .iter()
            .all(|item| item["orphaned"].is_null() && item["available_aliases"].is_array()));
    }

    #[test]
    fn dynamic_playlist_crud_is_transactional_and_rejects_path_membership() {
        let directory = tempfile::tempdir().unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let handle = make_playlist_handle(
            playlists_path.clone(),
            Arc::new(Mutex::new(PlaylistStore::default())),
        );

        handle.playlist_create("smart", true).unwrap();
        assert!(load_playlists(&playlists_path)
            .unwrap()
            .get("smart")
            .is_some());
        assert!(load_content(&directory.path().join("content.json"))
            .unwrap()
            .is_dynamic_playlist("smart"));
        assert!(handle
            .playlist_autotag_status("smart", "photo.bmp")
            .unwrap_err()
            .to_string()
            .contains("dynamic"));

        let error = handle
            .playlist_add("smart", "photo.bmp")
            .unwrap_err()
            .to_string();
        assert!(error.contains("dynamic"));
        assert!(error.contains("aurora-ctl content"));
        assert!(handle
            .playlist_autotag_upsert(
                "smart",
                "photo.bmp",
                BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                None,
                None,
                None,
                false,
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("dynamic"));

        handle
            .playlist_filter(
                "smart",
                BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();
        handle.playlist_delete("smart").unwrap();
        let content = load_content(&directory.path().join("content.json")).unwrap();
        assert!(!content.is_dynamic_playlist("smart"));
        assert!(content.playlist_filters("smart").is_none());
        assert!(load_playlists(&playlists_path)
            .unwrap()
            .get("smart")
            .is_none());
        assert!(!directory
            .path()
            .join(PLAYLIST_CONTENT_TRANSACTION_FILENAME)
            .exists());

        handle
            .content_store
            .lock()
            .set_dynamic_playlist("stale", true)
            .unwrap();
        assert!(handle
            .playlist_autotag_upsert(
                "stale",
                "photo.bmp",
                BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                None,
                None,
                None,
                true,
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("dynamic"));
        assert!(handle.playlist_store.lock().get("stale").is_none());
    }

    #[test]
    fn playlist_persist_failure_keeps_memory_unchanged() {
        let blocker = tempfile::NamedTempFile::new().unwrap();
        let store = Arc::new(Mutex::new(PlaylistStore::default()));
        let handle = make_playlist_handle(blocker.path().join("playlists.kdl"), Arc::clone(&store));

        assert!(handle.playlist_create("not-persisted", false).is_err());
        assert!(store.lock().get("not-persisted").is_none());
    }

    #[test]
    fn cross_store_validation_rejects_stale_or_pathful_dynamic_state() {
        let mut playlists = PlaylistStore::default();
        playlists.create("smart").unwrap();
        playlists.add_path("smart", "photo.jpg").unwrap();
        let mut content = ContentStore::default();
        content.set_dynamic_playlist("smart", true).unwrap();
        assert!(validate_playlist_content_consistency(&playlists, &content)
            .unwrap_err()
            .to_string()
            .contains("must not contain path membership"));

        let playlists = PlaylistStore::default();
        let mut content = ContentStore::default();
        content.set_dynamic_playlist("missing", true).unwrap();
        assert!(validate_playlist_content_consistency(&playlists, &content)
            .unwrap_err()
            .to_string()
            .contains("does not define it"));

        let mut content = ContentStore::default();
        content
            .set_playlist_filters(
                "missing",
                BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();
        assert!(validate_playlist_content_consistency(&playlists, &content)
            .unwrap_err()
            .to_string()
            .contains("filters for missing playlist"));
    }

    #[test]
    fn committed_playlist_content_transaction_recovers_every_install_point() {
        for installed_targets in 0..=2 {
            let directory = tempfile::tempdir().unwrap();
            let playlists_path = directory.path().join("playlists.kdl");
            let content_path = directory.path().join("content.json");

            let mut old_playlists = PlaylistStore::default();
            old_playlists.create("old").unwrap();
            persist_playlists(&old_playlists, &playlists_path).unwrap();
            persist_content(&ContentStore::default(), &content_path).unwrap();

            let mut next_playlists = PlaylistStore::default();
            next_playlists.create("next").unwrap();
            let hash = "a".repeat(64);
            let mut next_content = ContentStore::default();
            next_content
                .set_tag_group(
                    &hash,
                    &["next.jpg".to_string()],
                    "theme",
                    vec!["night".to_string()],
                    (None, None),
                )
                .unwrap();
            let transaction = PlaylistContentTransaction {
                version: PLAYLIST_CONTENT_TRANSACTION_VERSION,
                content_json: String::from_utf8(serialize_content(&next_content).unwrap()).unwrap(),
                playlists_kdl: serialize_playlists_checked(&next_playlists).unwrap(),
            };
            stage_playlist_content_transaction(&transaction, &playlists_path, &content_path)
                .unwrap();
            let transaction_path = playlist_content_transaction_path(&content_path);
            let transaction_tmp = transaction_path.with_extension("json.tmp");
            write_synced(&transaction_tmp, &serde_json::to_vec(&transaction).unwrap()).unwrap();
            crate::playlist::replace_file(&transaction_tmp, &transaction_path).unwrap();

            if installed_targets >= 1 {
                crate::playlist::replace_file(
                    &content_path.with_extension("json.tmp"),
                    &content_path,
                )
                .unwrap();
            }
            if installed_targets >= 2 {
                crate::playlist::replace_file(
                    &playlists_path.with_extension("kdl.tmp"),
                    &playlists_path,
                )
                .unwrap();
            }

            recover_playlist_content_transaction(&playlists_path, &content_path).unwrap();
            assert!(load_playlists(&playlists_path)
                .unwrap()
                .get("next")
                .is_some());
            assert_eq!(
                load_content(&content_path)
                    .unwrap()
                    .get(&hash)
                    .unwrap()
                    .tag_groups["theme"],
                ["night"]
            );
            assert!(!transaction_path.exists());
        }
    }

    #[test]
    fn invalid_playlist_content_transaction_fails_before_install() {
        let directory = tempfile::tempdir().unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let content_path = directory.path().join("content.json");
        let mut old_playlists = PlaylistStore::default();
        old_playlists.create("old").unwrap();
        persist_playlists(&old_playlists, &playlists_path).unwrap();
        persist_content(&ContentStore::default(), &content_path).unwrap();
        let old_playlists_bytes = std::fs::read(&playlists_path).unwrap();
        let old_content_bytes = std::fs::read(&content_path).unwrap();

        let transaction_path = playlist_content_transaction_path(&content_path);
        write_synced(
            &transaction_path,
            br#"{"version":99,"content_json":"{}","playlists_kdl":""}"#,
        )
        .unwrap();

        assert!(
            recover_playlist_content_transaction(&playlists_path, &content_path)
                .unwrap_err()
                .to_string()
                .contains("unsupported playlist/content transaction version")
        );
        assert_eq!(std::fs::read(&playlists_path).unwrap(), old_playlists_bytes);
        assert_eq!(std::fs::read(&content_path).unwrap(), old_content_bytes);
        assert!(transaction_path.exists());
    }

    #[test]
    fn concurrent_playlist_mutations_do_not_lose_updates() {
        let directory = tempfile::tempdir().unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let mut initial = PlaylistStore::default();
        initial.create("shared").unwrap();
        let store = Arc::new(Mutex::new(initial));
        let handle = make_playlist_handle(playlists_path.clone(), Arc::clone(&store));

        std::thread::scope(|scope| {
            for index in 0..8 {
                let handle = handle.clone();
                scope.spawn(move || {
                    handle
                        .playlist_add("shared", &format!("photo-{index}.jpg"))
                        .unwrap();
                });
            }
        });

        assert_eq!(store.lock().get("shared").unwrap().paths.len(), 8);
        assert_eq!(
            load_playlists(&playlists_path)
                .unwrap()
                .get("shared")
                .unwrap()
                .paths
                .len(),
            8
        );
    }

    #[test]
    fn playlist_shuffle_persists() {
        let directory = tempfile::tempdir().unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let mut initial = PlaylistStore::default();
        initial.create("focus").unwrap();
        let store = Arc::new(Mutex::new(initial));
        let handle = make_playlist_handle(playlists_path.clone(), Arc::clone(&store));

        handle.playlist_shuffle("focus", true).unwrap();

        assert!(store.lock().get("focus").unwrap().shuffle);
        assert!(
            load_playlists(&playlists_path)
                .unwrap()
                .get("focus")
                .unwrap()
                .shuffle
        );
    }

    #[test]
    fn exact_duplicates_share_content_tags_and_replaced_bytes_do_not() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.bmp");
        let duplicate = directory.path().join("duplicate.bmp");
        write_test_bmp(&first, [255, 0, 0]);
        std::fs::copy(&first, &duplicate).unwrap();
        let mut playlists = PlaylistStore::default();
        playlists.create("one").unwrap();
        playlists.create("two").unwrap();
        playlists.add_path("one", &first.to_string_lossy()).unwrap();
        playlists
            .add_path("two", &duplicate.to_string_lossy())
            .unwrap();
        let store = Arc::new(Mutex::new(playlists));
        let handle =
            make_playlist_handle(directory.path().join("playlists.kdl"), Arc::clone(&store));
        *handle.index.write() = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        *handle.source_roots.write() = vec![directory.path().to_path_buf()];

        handle
            .playlist_tag(
                "one",
                &first.to_string_lossy(),
                "theme",
                vec!["night".to_string()],
            )
            .unwrap();

        let first_page = handle.playlist_show("one", 0, 1).unwrap();
        let duplicate_page = handle.playlist_show("two", 0, 1).unwrap();
        assert_eq!(
            first_page["items"][0]["content_id"],
            duplicate_page["items"][0]["content_id"]
        );
        assert_eq!(
            duplicate_page["items"][0]["tag_groups"]["theme"][0],
            "night"
        );

        write_test_bmp(&duplicate, [0, 0, 255]);
        *handle.index.write() = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        let replaced_page = handle.playlist_show("two", 0, 1).unwrap();
        assert_ne!(
            first_page["items"][0]["content_id"],
            replaced_page["items"][0]["content_id"]
        );
        assert!(replaced_page["items"][0]["tag_groups"]
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn renamed_content_resolves_through_its_hash_and_keeps_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let old_path = directory.path().join("old.bmp");
        let new_path = directory.path().join("renamed.bmp");
        write_test_bmp(&old_path, [255, 0, 0]);
        let mut playlists = PlaylistStore::default();
        playlists.create("focus").unwrap();
        playlists
            .add_path("focus", &old_path.to_string_lossy())
            .unwrap();
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(playlists)),
        );
        *handle.index.write() = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        *handle.source_roots.write() = vec![directory.path().to_path_buf()];
        handle
            .playlist_tag(
                "focus",
                &old_path.to_string_lossy(),
                "theme",
                vec!["night".to_string()],
            )
            .unwrap();

        std::fs::rename(&old_path, &new_path).unwrap();
        *handle.index.write() = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();

        let page = handle.playlist_show("focus", 0, 1).unwrap();
        assert_eq!(
            page["items"][0]["resolved_path"],
            new_path.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(page["items"][0]["tag_groups"]["theme"][0], "night");
    }

    #[test]
    fn one_time_legacy_migration_unions_tags_without_reviving_them_later() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("shared.bmp");
        write_test_bmp(&image, [255, 0, 0]);
        let index = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        let id = index.photos[0].hash.clone();
        let path = image.to_string_lossy().into_owned();
        let mut playlists = PlaylistStore::default();
        for (name, tag, rating) in [("one", "night", 2), ("two", "city", 5)] {
            playlists.create(name).unwrap();
            playlists.add_path(name, &path).unwrap();
            playlists
                .set_tag_group(name, &path, "theme", vec![tag.to_string()])
                .unwrap();
            playlists.set_rating(name, &path, rating).unwrap();
        }
        let mut content = ContentStore::default();

        assert!(migrate_legacy_content(
            &mut content,
            &playlists,
            &index,
            &[directory.path().to_path_buf()]
        )
        .unwrap());
        let metadata = content.get(&id).unwrap();
        assert_eq!(metadata.tag_groups["theme"], ["city", "night"]);
        assert!(metadata.rating_conflicted);
        content
            .set_tag_group(
                &id,
                &[],
                "theme",
                vec!["day".to_string()],
                (Some(16), Some(16)),
            )
            .unwrap();

        assert!(!migrate_legacy_content(
            &mut content,
            &playlists,
            &index,
            &[directory.path().to_path_buf()]
        )
        .unwrap());
        assert_eq!(content.get(&id).unwrap().tag_groups["theme"], ["day"]);
    }

    #[test]
    fn schema_two_reconciliation_imports_only_previously_skipped_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let known = directory.path().join("known.bmp");
        let skipped = directory.path().join("skipped.bmp");
        write_test_bmp(&known, [255, 0, 0]);
        write_test_bmp(&skipped, [0, 0, 255]);
        let index = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        let known_id = index
            .photos
            .iter()
            .find(|entry| entry.path == known)
            .unwrap()
            .hash
            .clone();
        let skipped_id = index
            .photos
            .iter()
            .find(|entry| entry.path == skipped)
            .unwrap()
            .hash
            .clone();
        let known = known.to_string_lossy().into_owned();
        let skipped = skipped.to_string_lossy().into_owned();
        let mut playlists = PlaylistStore::default();
        playlists.create("focus").unwrap();
        playlists.add_path("focus", &known).unwrap();
        playlists.add_path("focus", &skipped).unwrap();
        playlists
            .set_tag_group("focus", &known, "theme", vec!["night".to_string()])
            .unwrap();
        playlists
            .set_tag_group("focus", &skipped, "artist", vec!["studio".to_string()])
            .unwrap();

        let metadata_path = directory.path().join("content.json");
        let entries = BTreeMap::from([(
            known_id.clone(),
            serde_json::json!({
                "aliases": [known],
                "tag_groups": {"theme": ["day"]}
            }),
        )]);
        std::fs::write(
            &metadata_path,
            serde_json::to_vec(&serde_json::json!({
                "schema": 2,
                "legacy_migrated": true,
                "entries": entries
            }))
            .unwrap(),
        )
        .unwrap();
        let mut content = load_content(&metadata_path).unwrap();
        assert!(content.needs_legacy_reconciliation());

        assert!(migrate_legacy_content(&mut content, &playlists, &index, &[]).unwrap());
        assert!(!content.needs_legacy_reconciliation());
        assert_eq!(content.get(&known_id).unwrap().tag_groups["theme"], ["day"]);
        assert_eq!(
            content.get(&skipped_id).unwrap().tag_groups["artist"],
            ["studio"]
        );
        assert!(!migrate_legacy_content(&mut content, &playlists, &index, &[]).unwrap());

        persist_content(&content, &metadata_path).unwrap();
        assert!(!load_content(&metadata_path)
            .unwrap()
            .needs_legacy_reconciliation());
    }

    #[test]
    fn deferred_legacy_migration_retries_without_hiding_local_groups() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("offline.bmp");
        let unreadable = directory.path().join("unreadable.bmp");
        std::fs::write(&unreadable, b"not an image").unwrap();
        let stored = image.to_string_lossy().into_owned();
        let unreadable_stored = unreadable.to_string_lossy().into_owned();
        let mut playlists = PlaylistStore::default();
        playlists.create("focus").unwrap();
        playlists.add_path("focus", &stored).unwrap();
        playlists.add_path("focus", &unreadable_stored).unwrap();
        playlists
            .set_tag_group("focus", &stored, "theme", vec!["night".to_string()])
            .unwrap();
        playlists
            .set_tag_group("focus", &stored, "artist", vec!["studio".to_string()])
            .unwrap();
        playlists
            .set_tag_group(
                "focus",
                &unreadable_stored,
                "content",
                vec!["city".to_string()],
            )
            .unwrap();
        let mut content = ContentStore::default();

        assert!(
            migrate_legacy_content(&mut content, &playlists, &PhotoIndex::default(), &[]).unwrap()
        );
        assert!(!content.needs_legacy_migration());
        assert!(content.is_legacy_pending("focus", &stored));
        assert!(content.is_legacy_pending("focus", &unreadable_stored));

        write_test_bmp(&image, [255, 0, 0]);
        write_test_bmp(&unreadable, [0, 0, 255]);
        let index = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        let id = index.photos[0].hash.clone();
        content
            .set_tag_group(
                &id,
                std::slice::from_ref(&stored),
                "color",
                vec!["blue".to_string()],
                (Some(16), Some(16)),
            )
            .unwrap();
        content
            .set_playlist_filters(
                "focus",
                BTreeMap::from([("artist".to_string(), vec!["studio".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();

        let playlist = playlists.get("focus").unwrap();
        let metadata = content.get(&id).unwrap();
        let groups = effective_tag_groups(playlist, &stored, Some(metadata), true);
        assert_eq!(groups["theme"], ["night"]);
        assert_eq!(groups["artist"], ["studio"]);
        assert_eq!(groups["color"], ["blue"]);
        assert!(content.playlist_accepts("focus", Some(&groups)));
        let item = playlist_item_json(playlist, &stored, None, Some(metadata), true);
        assert_eq!(item["tag_groups"]["artist"][0], "studio");

        assert!(migrate_legacy_content(&mut content, &playlists, &index, &[]).unwrap());
        assert!(!content.is_legacy_pending("focus", &stored));
        assert!(!content.is_legacy_pending("focus", &unreadable_stored));
        let metadata = content.get(&id).unwrap();
        assert_eq!(metadata.tag_groups["theme"], ["night"]);
        assert_eq!(metadata.tag_groups["artist"], ["studio"]);
        assert_eq!(metadata.tag_groups["color"], ["blue"]);
        let unreadable_id = index
            .photos
            .iter()
            .find(|entry| entry.path == unreadable)
            .unwrap()
            .hash
            .clone();
        assert_eq!(
            content.get(&unreadable_id).unwrap().tag_groups["content"],
            ["city"]
        );
    }

    #[test]
    fn playlist_tag_filters_persist_and_gate_selection() {
        let directory = tempfile::tempdir().unwrap();
        let night = directory.path().join("night.bmp");
        let day = directory.path().join("day.bmp");
        write_test_bmp(&night, [0, 0, 0]);
        write_test_bmp(&day, [255, 255, 255]);
        let mut playlists = PlaylistStore::default();
        playlists.create("focus").unwrap();
        playlists
            .add_path("focus", &night.to_string_lossy())
            .unwrap();
        playlists.add_path("focus", &day.to_string_lossy()).unwrap();
        playlists.activate("focus").unwrap();
        let store = Arc::new(Mutex::new(playlists));
        let handle =
            make_playlist_handle(directory.path().join("playlists.kdl"), Arc::clone(&store));
        *handle.index.write() = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        *handle.source_roots.write() = vec![directory.path().to_path_buf()];
        handle
            .playlist_tag(
                "focus",
                &night.to_string_lossy(),
                "theme",
                vec!["night".to_string()],
            )
            .unwrap();
        handle
            .playlist_tag(
                "focus",
                &day.to_string_lossy(),
                "theme",
                vec!["day".to_string()],
            )
            .unwrap();
        handle
            .playlist_filter(
                "focus",
                BTreeMap::from([("theme".to_string(), vec!["day".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();

        let index = handle.index.read();
        let roots = handle.source_roots.read();
        let content = handle.content_store.lock();
        let picked = store.lock().pick_resolved(
            &mut HashMap::new(),
            0,
            &VecDeque::new(),
            &HashSet::new(),
            |playlist, stored| {
                let identity = resolve_content(&index, &content, stored, &roots, false).ok()??;
                let metadata = content.get(&identity.hash);
                content
                    .playlist_accepts(
                        &playlist.name,
                        metadata.map(|metadata| &metadata.tag_groups),
                    )
                    .then_some((identity.path, metadata.and_then(|metadata| metadata.rating)))
            },
        );
        drop(content);
        drop(roots);
        drop(index);

        assert_eq!(picked, Some(day.canonicalize().unwrap()));
        let persisted = load_content(handle.content_path.as_ref()).unwrap();
        assert_eq!(
            persisted.playlist_filters("focus").unwrap().include["theme"],
            ["day"]
        );
        assert_eq!(
            handle.playlist_list()["playlists"][0]["include_tags"]["theme"][0],
            "day"
        );

        handle.playlist_delete("focus").unwrap();
        assert!(handle
            .content_store
            .lock()
            .playlist_filters("focus")
            .is_none());
        assert!(load_content(handle.content_path.as_ref())
            .unwrap()
            .playlist_filters("focus")
            .is_none());
    }

    #[test]
    fn content_persist_failure_keeps_shared_metadata_memory_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("valid.bmp");
        write_test_bmp(&image, [255, 0, 0]);
        let blocker = tempfile::NamedTempFile::new().unwrap();
        let mut playlists = PlaylistStore::default();
        playlists.create("focus").unwrap();
        playlists
            .add_path("focus", &image.to_string_lossy())
            .unwrap();
        let handle = make_playlist_handle(
            blocker.path().join("playlists.kdl"),
            Arc::new(Mutex::new(playlists)),
        );
        *handle.index.write() = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();

        assert!(handle
            .playlist_tag(
                "focus",
                &image.to_string_lossy(),
                "theme",
                vec!["night".to_string()],
            )
            .is_err());
        let hash = handle.index.read().photos[0].hash.clone();
        assert!(handle.content_store.lock().get(&hash).is_none());
    }

    #[test]
    fn playlist_show_paginates_all_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let mut initial = PlaylistStore::default();
        initial.create("focus").unwrap();
        for path in ["a.jpg", "b.jpg", "c.jpg"] {
            initial.add_path("focus", path).unwrap();
        }
        initial
            .set_tag_group("focus", "b.jpg", "theme", vec!["night".to_string()])
            .unwrap();
        initial
            .set_tag_group("focus", "b.jpg", "artist", vec!["studio".to_string()])
            .unwrap();
        initial.set_rating("focus", "b.jpg", 4).unwrap();
        initial.set_frequency("focus", "b.jpg", 2).unwrap();
        initial.activate("focus").unwrap();
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(initial)),
        );

        let page = handle.playlist_show("focus", 1, 1).unwrap();
        assert_eq!(page["playlist"]["name"], "focus");
        assert_eq!(page["playlist"]["path_count"], 3);
        assert_eq!(page["playlist"]["active"], true);
        assert_eq!(page["total"], 3);
        assert_eq!(page["offset"], 1);
        assert_eq!(page["limit"], 1);
        assert_eq!(page["next_offset"], 2);
        assert_eq!(page["items"][0]["path"], "b.jpg");
        assert_eq!(
            page["items"][0]["tag_groups"]["theme"],
            serde_json::json!(["night"])
        );
        assert_eq!(
            page["items"][0]["tag_groups"]["artist"],
            serde_json::json!(["studio"])
        );
        assert_eq!(page["items"][0]["tag_groups"].as_object().unwrap().len(), 2);
        assert!(page["items"][0].get("tags").is_none());
        assert!(page["items"][0]["tag_groups"].get("general").is_none());
        assert_eq!(page["items"][0]["rating"], 4);
        assert_eq!(page["items"][0]["frequency"], 2);

        let end = handle.playlist_show("focus", 2, 256).unwrap();
        assert_eq!(end["items"][0]["path"], "c.jpg");
        assert!(end["next_offset"].is_null());
        assert!(handle.playlist_show("focus", 0, 0).is_err());
        assert!(handle.playlist_show("focus", 0, 257).is_err());
        assert!(handle.playlist_show("missing", 0, 1).is_err());
    }

    #[test]
    fn playlist_show_wire_limit_is_inclusive() {
        let empty = serde_json::json!({ "padding": "" });
        let overhead = playlist_show_wire_len(&empty).unwrap();
        let exact = serde_json::json!({ "padding": "x".repeat(MAX_FRAME_SIZE - overhead) });
        let oversized = serde_json::json!({ "padding": "x".repeat(MAX_FRAME_SIZE + 1 - overhead) });

        assert_eq!(playlist_show_wire_len(&exact).unwrap(), MAX_FRAME_SIZE);
        assert!(playlist_show_fits_frame(&exact).unwrap());
        assert!(!playlist_show_fits_frame(&oversized).unwrap());
    }

    #[test]
    fn playlist_show_retrieves_near_limit_item_and_rejects_oversized_item() {
        let directory = tempfile::tempdir().unwrap();
        let mut initial = PlaylistStore::default();
        initial.create("large-tags").unwrap();
        initial.add_path("large-tags", "near.jpg").unwrap();
        initial.add_path("large-tags", "too-large.jpg").unwrap();
        initial
            .set_tags(
                "large-tags",
                "near.jpg",
                vec!["x".repeat(MAX_FRAME_SIZE - 2_048)],
            )
            .unwrap();
        initial
            .set_tags(
                "large-tags",
                "too-large.jpg",
                vec!["x".repeat(MAX_FRAME_SIZE)],
            )
            .unwrap();
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(initial)),
        );

        let page = handle.playlist_show("large-tags", 0, 1).unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert!(page["items"][0].get("tags").is_none());
        let wire_len = playlist_show_wire_len(&page).unwrap();
        assert!(wire_len < MAX_FRAME_SIZE);
        assert!(wire_len > MAX_FRAME_SIZE - 4_096);

        let error = handle
            .playlist_show("large-tags", 1, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("offset 1"));
        assert!(error.contains("reduce its tag metadata"));
    }

    #[test]
    fn playlist_show_truncates_multi_item_pages_to_the_wire_budget() {
        let directory = tempfile::tempdir().unwrap();
        let mut initial = PlaylistStore::default();
        initial.create("chunked").unwrap();
        let tag = "x".repeat(400_000);
        for index in 0..3 {
            let path = format!("{index}.jpg");
            initial.add_path("chunked", &path).unwrap();
            initial
                .set_tags("chunked", &path, vec![tag.clone()])
                .unwrap();
        }
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(initial)),
        );

        let page = handle.playlist_show("chunked", 0, 3).unwrap();
        assert_eq!(page["offset"], 0);
        assert_eq!(page["limit"], 3);
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert_eq!(page["next_offset"], 2);
        assert!(playlist_show_wire_len(&page).unwrap() < MAX_FRAME_SIZE);

        let next = handle.playlist_show("chunked", 2, 1).unwrap();
        assert_eq!(next["items"][0]["path"], "2.jpg");
        assert!(next["next_offset"].is_null());
    }

    #[test]
    fn playlist_list_stays_compact_for_a_large_playlist() {
        let directory = tempfile::tempdir().unwrap();
        let mut initial = PlaylistStore::default();
        initial.create("large").unwrap();
        initial.get_mut("large").unwrap().paths =
            (0..100_000).map(|index| format!("{index}.jpg")).collect();
        initial.activate("large").unwrap();
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(initial)),
        );

        let list = handle.playlist_list();
        let summary = &list["playlists"][0];
        assert_eq!(list["active"], "large");
        assert_eq!(summary["path_count"], 100_000);
        assert_eq!(summary["active"], true);
        assert!(summary.get("paths").is_none());
        assert!(summary.get("items").is_none());
        assert!(serde_json::to_vec(&list).unwrap().len() < 256);
    }

    #[test]
    fn absolute_requests_update_one_legacy_relative_playlist_entry() {
        let directory = tempfile::tempdir().unwrap();
        let photo = directory.path().join("photo.jpg");
        std::fs::write(&photo, b"photo").unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let mut initial = PlaylistStore::default();
        initial.create("legacy").unwrap();
        initial.add_path("legacy", "photo.jpg").unwrap();
        let store = Arc::new(Mutex::new(initial));
        let handle = make_playlist_handle(playlists_path, Arc::clone(&store));
        *handle.source_roots.write() = vec![directory.path().to_path_buf()];
        let absolute = std::fs::canonicalize(&photo)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        handle.playlist_add("legacy", &absolute).unwrap();
        handle
            .playlist_tag("legacy", &absolute, "theme", vec!["night".to_string()])
            .unwrap();
        handle.playlist_rate("legacy", &absolute, 4).unwrap();
        handle.playlist_frequency("legacy", &absolute, 2).unwrap();
        assert!(handle.playlist_autotag_status("legacy", &absolute).unwrap());
        handle
            .playlist_autotag_upsert(
                "legacy",
                &absolute,
                BTreeMap::from([
                    ("theme".to_string(), vec!["night".to_string()]),
                    ("content".to_string(), vec!["city".to_string()]),
                ]),
                Some(4),
                Some(2),
                None,
                false,
                true,
            )
            .unwrap();

        let current = store.lock();
        let playlist = current.get("legacy").unwrap();
        assert_eq!(playlist.paths, ["photo.jpg"]);
        assert_eq!(playlist.tag_groups["theme"]["photo.jpg"], ["night"]);
        assert_eq!(playlist.tag_groups["content"]["photo.jpg"], ["city"]);
        assert_eq!(playlist.ratings["photo.jpg"], 4);
        assert_eq!(playlist.frequencies["photo.jpg"], 2);
        drop(current);

        handle.playlist_remove("legacy", &absolute).unwrap();
        assert!(store.lock().get("legacy").unwrap().paths.is_empty());
    }

    #[test]
    fn absolute_request_does_not_alias_a_shadowed_relative_entry() {
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        std::fs::write(root_a.path().join("photo.jpg"), b"first").unwrap();
        let second = root_b.path().join("photo.jpg");
        std::fs::write(&second, b"second").unwrap();

        let mut initial = PlaylistStore::default();
        initial.create("roots").unwrap();
        initial.add_path("roots", "photo.jpg").unwrap();
        let store = Arc::new(Mutex::new(initial));
        let handle = make_playlist_handle(root_a.path().join("playlists.kdl"), Arc::clone(&store));
        *handle.source_roots.write() =
            vec![root_a.path().to_path_buf(), root_b.path().to_path_buf()];
        let second = std::fs::canonicalize(second)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let error = handle
            .playlist_tag("roots", &second, "theme", vec!["night".to_string()])
            .unwrap_err()
            .to_string();

        assert!(error.contains("not in playlist"), "{error}");
        assert!(store.lock().get("roots").unwrap().tag_groups.is_empty());
    }

    #[test]
    fn absolute_request_rejects_ambiguous_relative_entries() {
        let directory = tempfile::tempdir().unwrap();
        let photo = directory.path().join("photo.jpg");
        std::fs::write(&photo, b"photo").unwrap();
        let mut initial = PlaylistStore::default();
        initial.create("ambiguous").unwrap();
        initial.add_path("ambiguous", "photo.jpg").unwrap();
        initial.add_path("ambiguous", ".\\photo.jpg").unwrap();
        let store = Arc::new(Mutex::new(initial));
        let handle = make_playlist_handle(directory.path().join("playlists.kdl"), store);
        *handle.source_roots.write() = vec![directory.path().to_path_buf()];
        let absolute = std::fs::canonicalize(photo)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let error = handle
            .playlist_tag("ambiguous", &absolute, "theme", vec!["night".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("multiple entries"), "{error}");
    }

    #[test]
    fn exact_absolute_entry_wins_before_equivalent_relative_entry() {
        let directory = tempfile::tempdir().unwrap();
        let photo = directory.path().join("photo.jpg");
        std::fs::write(&photo, b"photo").unwrap();
        let absolute = std::fs::canonicalize(&photo)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut initial = PlaylistStore::default();
        initial.create("duplicates").unwrap();
        initial.add_path("duplicates", "photo.jpg").unwrap();
        initial.add_path("duplicates", &absolute).unwrap();
        let store = Arc::new(Mutex::new(initial));
        let handle =
            make_playlist_handle(directory.path().join("playlists.kdl"), Arc::clone(&store));
        *handle.source_roots.write() = vec![directory.path().to_path_buf()];

        handle.playlist_remove("duplicates", &absolute).unwrap();
        assert_eq!(store.lock().get("duplicates").unwrap().paths, ["photo.jpg"]);

        handle.playlist_remove("duplicates", &absolute).unwrap();
        assert!(store.lock().get("duplicates").unwrap().paths.is_empty());
    }

    #[test]
    fn absolute_request_resolves_missing_legacy_relative_entry() {
        let directory = tempfile::tempdir().unwrap();
        let absolute = directory
            .path()
            .join("missing.jpg")
            .to_string_lossy()
            .into_owned();
        let mut initial = PlaylistStore::default();
        initial.create("legacy").unwrap();
        initial.add_path("legacy", "missing.jpg").unwrap();
        let store = Arc::new(Mutex::new(initial));
        let handle =
            make_playlist_handle(directory.path().join("playlists.kdl"), Arc::clone(&store));
        *handle.source_roots.write() = vec![directory.path().to_path_buf()];

        handle.playlist_add("legacy", &absolute).unwrap();
        assert_eq!(store.lock().get("legacy").unwrap().paths, ["missing.jpg"]);

        handle.playlist_remove("legacy", &absolute).unwrap();
        assert!(store.lock().get("legacy").unwrap().paths.is_empty());
    }

    #[test]
    fn playlist_autotag_upsert_is_transactional_and_does_not_duplicate_paths() {
        let directory = tempfile::tempdir().unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let store = Arc::new(Mutex::new(PlaylistStore::default()));
        let handle = make_playlist_handle(playlists_path.clone(), Arc::clone(&store));
        let first = BTreeMap::from([
            ("theme".to_string(), vec!["night".to_string()]),
            ("character".to_string(), vec!["miku".to_string()]),
            ("artist".to_string(), vec!["kei".to_string()]),
        ]);

        assert!(handle
            .playlist_autotag_upsert(
                "auto",
                "photo.jpg",
                first,
                Some(4),
                Some(2),
                None,
                true,
                false,
            )
            .unwrap());
        assert!(handle.playlist_autotag_status("auto", "photo.jpg").unwrap());

        let replacement = BTreeMap::from([
            ("theme".to_string(), vec!["sunrise".to_string()]),
            ("unused".to_string(), Vec::new()),
        ]);
        assert!(!handle
            .playlist_autotag_upsert(
                "auto",
                "photo.jpg",
                replacement.clone(),
                Some(5),
                Some(3),
                None,
                false,
                false,
            )
            .unwrap());
        assert!(handle
            .playlist_autotag_upsert(
                "auto",
                "photo.jpg",
                replacement,
                None,
                None,
                None,
                false,
                true,
            )
            .unwrap());

        let persisted = load_playlists(&playlists_path).unwrap();
        let playlist = persisted.get("auto").unwrap();
        assert_eq!(playlist.paths, ["photo.jpg"]);
        assert_eq!(playlist.tag_groups["theme"]["photo.jpg"], ["sunrise"]);
        assert!(!playlist.tag_groups.contains_key("character"));
        assert!(!playlist.tag_groups.contains_key("artist"));
        assert!(!playlist.tag_groups.contains_key("unused"));
        assert!(!playlist.ratings.contains_key("photo.jpg"));
        assert!(!playlist.frequencies.contains_key("photo.jpg"));
    }

    #[test]
    fn playlist_autotag_rejects_effectively_empty_forced_update_before_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let mut initial = PlaylistStore::default();
        initial.create("auto").unwrap();
        initial.add_path("auto", "photo.jpg").unwrap();
        initial
            .set_tag_group("auto", "photo.jpg", "theme", vec!["night".to_string()])
            .unwrap();
        initial.set_rating("auto", "photo.jpg", 4).unwrap();
        let store = Arc::new(Mutex::new(initial));
        let handle = make_playlist_handle(playlists_path.clone(), Arc::clone(&store));

        let error = handle
            .playlist_autotag_upsert(
                "auto",
                "photo.jpg",
                BTreeMap::from([("theme".to_string(), Vec::new())]),
                None,
                None,
                None,
                false,
                true,
            )
            .unwrap_err()
            .to_string();

        assert!(error.contains("no tags, rating, or frequency"), "{error}");
        let playlist = store.lock();
        let playlist = playlist.get("auto").unwrap();
        assert_eq!(playlist.tag_groups["theme"]["photo.jpg"], ["night"]);
        assert_eq!(playlist.ratings["photo.jpg"], 4);
        assert!(!playlists_path.exists());
    }

    #[test]
    fn playlist_autotag_status_counts_frequency_only() {
        let directory = tempfile::tempdir().unwrap();
        let mut initial = PlaylistStore::default();
        initial.create("auto").unwrap();
        initial.add_path("auto", "photo.jpg").unwrap();
        initial.set_frequency("auto", "photo.jpg", 2).unwrap();
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(initial)),
        );

        assert!(handle.playlist_autotag_status("auto", "photo.jpg").unwrap());
    }

    #[test]
    fn playlist_autotag_failure_rolls_back_content_and_memory() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("photo.bmp");
        write_test_bmp(&image, [255, 0, 0]);
        let blocker = tempfile::NamedTempFile::new().unwrap();
        let store = Arc::new(Mutex::new(PlaylistStore::default()));
        let content = Arc::new(Mutex::new(ContentStore::default()));
        let index = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();
        let hash = index.photos[0].hash.clone();
        let (tx, _rx) = mpsc::channel(4);
        let config_path = directory.path().join("config.kdl");
        let handle = RuntimeHandle::new(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            RuntimeShared::new(
                Arc::new(RwLock::new(index)),
                Arc::new(RwLock::new(vec![directory.path().to_path_buf()])),
                BanGate::default(),
                Arc::clone(&content),
            ),
            Metrics::new(),
            config_path.clone(),
            Arc::clone(&store),
            blocker.path().join("playlists.kdl"),
        );

        assert!(handle
            .playlist_autotag_upsert(
                "auto",
                &image.to_string_lossy(),
                BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                None,
                None,
                Some(AutoTagProvenance {
                    model: "model".to_string(),
                    confidence: Some(0.8),
                    raw: serde_json::json!({"theme": ["night"]}),
                    ..Default::default()
                }),
                true,
                false,
            )
            .is_err());
        assert!(store.lock().get("auto").is_none());
        assert!(content.lock().get(&hash).is_none());
        assert!(!content_path(&config_path).exists());
        assert!(!playlist_content_transaction_path(handle.content_path.as_ref()).exists());
    }

    #[test]
    fn playlist_autotag_persists_content_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("photo.bmp");
        write_test_bmp(&image, [255, 0, 0]);
        let playlists_path = directory.path().join("playlists.kdl");
        let handle = make_playlist_handle(
            playlists_path,
            Arc::new(Mutex::new(PlaylistStore::default())),
        );
        *handle.index.write() = PhotoIndex::scan(
            &[directory.path().to_path_buf()],
            &["bmp".to_string()],
            false,
        )
        .unwrap();

        handle
            .playlist_autotag_upsert(
                "auto",
                &image.to_string_lossy(),
                BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                Some(4),
                None,
                Some(AutoTagProvenance {
                    model: "vision-model".to_string(),
                    confidence: Some(0.9),
                    raw: serde_json::json!({"identity": {"theme": ["night"]}}),
                    ..Default::default()
                }),
                true,
                false,
            )
            .unwrap();

        let item = handle.playlist_show("auto", 0, 1).unwrap()["items"][0].clone();
        assert_eq!(item["autotag"]["model"], "vision-model");
        assert_eq!(item["autotag"]["confidence"], 0.9);
        let hash = handle.index.read().photos[0].hash.clone();
        assert_eq!(
            load_content(handle.content_path.as_ref())
                .unwrap()
                .get(&hash)
                .unwrap()
                .autotag
                .as_ref()
                .unwrap()
                .model,
            "vision-model"
        );
    }

    #[test]
    fn playlist_autotag_rejects_invalid_input() {
        let directory = tempfile::tempdir().unwrap();
        let handle = make_playlist_handle(
            directory.path().join("playlists.kdl"),
            Arc::new(Mutex::new(PlaylistStore::default())),
        );
        let tags = || BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]);

        assert!(handle.playlist_autotag_status("", "photo.jpg").is_err());
        assert!(handle
            .playlist_autotag_upsert("auto", "", tags(), None, None, None, true, false)
            .is_err());
        assert!(handle
            .playlist_autotag_upsert(
                "auto",
                "photo.jpg",
                tags(),
                Some(6),
                None,
                None,
                true,
                false,
            )
            .is_err());
        assert!(handle
            .playlist_autotag_upsert(
                "auto",
                "photo.jpg",
                tags(),
                None,
                Some(0),
                None,
                true,
                false,
            )
            .is_err());
    }

    #[test]
    fn playlist_activation_succeeds_when_immediate_swap_queue_is_full() {
        let directory = tempfile::tempdir().unwrap();
        let playlists_path = directory.path().join("playlists.kdl");
        let mut initial = PlaylistStore::default();
        initial.create("focus").unwrap();
        let store = Arc::new(Mutex::new(initial));
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(SwapRequest {
            reason: SwapReason::Interval,
            specific: None,
        })
        .unwrap();
        let handle = RuntimeHandle::new(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            RuntimeShared::new(
                Arc::new(RwLock::new(PhotoIndex::default())),
                Arc::new(RwLock::new(Vec::new())),
                BanGate::default(),
                Arc::new(Mutex::new(ContentStore::default())),
            ),
            Metrics::new(),
            directory.path().join("config.kdl"),
            Arc::clone(&store),
            playlists_path.clone(),
        );

        handle.playlist_activate("focus").unwrap();

        assert_eq!(store.lock().active.as_deref(), Some("focus"));
        assert_eq!(
            load_playlists(&playlists_path).unwrap().active.as_deref(),
            Some("focus")
        );
    }

    #[test]
    fn reload_and_empty_set_folder_restore_configured_roots() {
        let directory = tempfile::tempdir().unwrap();
        let configured = directory.path().join("configured");
        let session = directory.path().join("session");
        std::fs::create_dir_all(&configured).unwrap();
        std::fs::create_dir_all(&session).unwrap();
        write_test_bmp(&configured.join("relative.bmp"), [255, 0, 0]);
        write_test_bmp(&session.join("session.bmp"), [0, 0, 255]);
        let config_path = directory.path().join("config.kdl");
        std::fs::write(&config_path, source_config(&configured)).unwrap();

        let roots = Arc::new(RwLock::new(vec![PathBuf::from("stale")]));
        let handle = make_source_handle(
            config_path,
            Arc::new(RwLock::new(PhotoIndex::default())),
            Arc::clone(&roots),
            Metrics::new(),
        );

        handle.reload_from_disk().unwrap();
        assert_eq!(*roots.read(), vec![configured.clone()]);

        let mut playlist = PlaylistStore::default();
        playlist.create("relative").unwrap();
        playlist.add_path("relative", "relative.bmp").unwrap();
        playlist.activate("relative").unwrap();
        let root_guard = roots.read();
        let root_refs: Vec<&Path> = root_guard.iter().map(PathBuf::as_path).collect();
        assert_eq!(
            playlist.pick_from_roots(
                &root_refs,
                &mut HashMap::new(),
                0,
                &VecDeque::new(),
                &HashSet::new(),
            ),
            Some(configured.join("relative.bmp"))
        );
        drop(root_guard);

        handle.set_folder(session.clone()).unwrap();
        assert_eq!(*roots.read(), vec![session]);
        handle.set_folder(PathBuf::new()).unwrap();
        assert_eq!(*roots.read(), vec![configured]);
    }

    #[test]
    fn reload_refreshes_external_playlist_and_content_edits() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.kdl");
        let playlists_path = directory.path().join("playlists.kdl");
        std::fs::write(&config_path, "").unwrap();

        let mut external_playlists = PlaylistStore::default();
        external_playlists.create("external").unwrap();
        persist_playlists(&external_playlists, &playlists_path).unwrap();

        let mut external_content = ContentStore::default();
        external_content
            .set_playlist_filters(
                "external",
                std::collections::BTreeMap::from([(
                    "theme".to_string(),
                    vec!["night".to_string()],
                )]),
                std::collections::BTreeMap::new(),
            )
            .unwrap();
        persist_content(&external_content, &content_path(&config_path)).unwrap();

        let playlists = Arc::new(Mutex::new(PlaylistStore::default()));
        let content = Arc::new(Mutex::new(ContentStore::default()));
        let (tx, _rx) = mpsc::channel(4);
        let handle = RuntimeHandle::new(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            RuntimeShared::new(
                Arc::new(RwLock::new(PhotoIndex::default())),
                Arc::new(RwLock::new(Vec::new())),
                BanGate::default(),
                Arc::clone(&content),
            ),
            Metrics::new(),
            config_path,
            Arc::clone(&playlists),
            playlists_path,
        );

        handle.reload_from_disk().unwrap();

        assert!(playlists.lock().get("external").is_some());
        assert_eq!(
            content.lock().playlist_filters("external").unwrap().include["theme"],
            ["night"]
        );
    }

    #[test]
    fn set_folder_uses_the_bundled_default_extension_policy() {
        let directory = tempfile::tempdir().unwrap();
        write_test_bmp(&directory.path().join("wallpaper.bmp"), [255, 0, 0]);
        let roots = Arc::new(RwLock::new(Vec::new()));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let handle = make_source_handle(
            directory.path().join("config.kdl"),
            Arc::clone(&index),
            Arc::clone(&roots),
            Metrics::new(),
        );

        handle.set_folder(directory.path().to_path_buf()).unwrap();

        assert_eq!(index.read().len(), 1);
        assert_eq!(*roots.read(), vec![directory.path().to_path_buf()]);
    }

    #[test]
    fn reload_initializes_com_for_wic_only_extensions() {
        use image::{ImageBuffer, ImageFormat, Rgb};

        let directory = tempfile::tempdir().unwrap();
        let image_path = directory.path().join("wallpaper.heic");
        let image: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(16, 16, Rgb([255, 0, 0]));
        image
            .save_with_format(&image_path, ImageFormat::Bmp)
            .unwrap();
        let config_path = directory.path().join("config.kdl");
        std::fs::write(
            &config_path,
            format!(
                "source {{\npath \"{}\"\nextensions \"heic\"\nmin-width 0\nmin-height 0\n}}\n",
                directory.path().display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let handle = make_source_handle(
            config_path,
            Arc::clone(&index),
            Arc::new(RwLock::new(Vec::new())),
            Metrics::new(),
        );

        handle.reload_from_disk().unwrap();

        assert_eq!(index.read().len(), 1);
    }

    #[test]
    fn failed_source_updates_leave_index_and_roots_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.kdl");
        std::fs::write(&config_path, "source {\n").unwrap();
        let mut initial_index = PhotoIndex::default();
        initial_index.photos.push(crate::index::PhotoEntry {
            path: PathBuf::from("sentinel.jpg"),
            width: None,
            height: None,
            hash: "sentinel".to_string(),
            banned: false,
        });
        let index = Arc::new(RwLock::new(initial_index));
        let roots = Arc::new(RwLock::new(vec![PathBuf::from("sentinel-root")]));
        let metrics = Metrics::new();
        metrics.set_index_size(1);
        let handle = make_source_handle(
            config_path,
            Arc::clone(&index),
            Arc::clone(&roots),
            Arc::clone(&metrics),
        );

        assert!(handle.reload_from_disk().is_err());
        assert!(handle.set_folder(directory.path().join("missing")).is_err());

        assert_eq!(index.read().photos[0].path, PathBuf::from("sentinel.jpg"));
        assert_eq!(*roots.read(), vec![PathBuf::from("sentinel-root")]);
        assert_eq!(metrics.index_size.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reload_preparation_does_not_wait_for_ban_updates() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.kdl");
        std::fs::write(
            &config_path,
            source_config(&directory.path().join("missing-source")),
        )
        .unwrap();
        let handle = make_source_handle(
            config_path,
            Arc::new(RwLock::new(PhotoIndex::default())),
            Arc::new(RwLock::new(Vec::new())),
            Metrics::new(),
        );
        let ban_update = handle.ban_gate.0.updates.lock();
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        let result = std::thread::scope(|scope| {
            let handle = handle.clone();
            scope.spawn(move || {
                result_tx
                    .send(handle.reload_from_disk().map_err(|error| error.to_string()))
                    .unwrap();
            });
            let result = result_rx.recv_timeout(Duration::from_secs(1));
            drop(ban_update);
            result
        });

        assert!(
            matches!(&result, Ok(Err(error)) if error.contains("scanning photo sources")),
            "source scan waited for the ban update lock: {result:?}"
        );
    }

    #[test]
    fn ban_sidecar_roundtrips_and_reapplies_hashes() {
        let directory = tempfile::tempdir().unwrap();
        let path = bans_path(&directory.path().join("config.kdl"));
        let hash = "A".repeat(64);
        let normalized = normalize_ban_hash(&hash).unwrap();
        let bans = HashSet::from([normalized.clone()]);
        persist_bans(&path, &bans).unwrap();
        let loaded = load_bans(&path).unwrap();
        assert_eq!(loaded, bans);

        let mut index = PhotoIndex::default();
        index.photos.push(crate::index::PhotoEntry {
            path: PathBuf::from("photo.jpg"),
            width: None,
            height: None,
            hash: normalized.clone(),
            banned: false,
        });
        assert_eq!(index.apply_bans(&loaded), 1);
        assert!(index.photos[0].banned);
    }

    #[test]
    fn banned_specific_path_is_rejected_before_enqueue() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("banned.jpg");
        std::fs::write(&path, b"banned wallpaper").unwrap();
        let hash = crate::index::hash_file(&path).unwrap();
        let mut index = PhotoIndex::default();
        index.photos.push(crate::index::PhotoEntry {
            path: path.clone(),
            width: None,
            height: None,
            hash: hash.clone(),
            banned: false,
        });
        let (tx, mut rx) = mpsc::channel(1);
        let handle = RuntimeHandle::new(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            RuntimeShared::new(
                Arc::new(RwLock::new(index)),
                Arc::new(RwLock::new(Vec::new())),
                BanGate::default(),
                Arc::new(Mutex::new(ContentStore::default())),
            ),
            Metrics::new(),
            directory.path().join("config.kdl"),
            Arc::new(Mutex::new(PlaylistStore::default())),
            directory.path().join("playlists.kdl"),
        );

        handle.ban(&hash).unwrap();
        let error = handle.set_specific(path).unwrap_err().to_string();
        assert!(error.contains("wallpaper is banned"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn out_of_index_hash_ban_persists_and_blocks_external_playlist_file() {
        let directory = tempfile::tempdir().unwrap();
        let external = directory.path().join("external.jpg");
        let future_match = directory.path().join("future.jpg");
        std::fs::write(&external, b"external playlist wallpaper").unwrap();
        std::fs::write(&future_match, b"external playlist wallpaper").unwrap();
        let hash = crate::index::hash_file(&external).unwrap();
        let config_path = directory.path().join("config.kdl");
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let gate = BanGate::default();
        let mut playlists = PlaylistStore::default();
        playlists.create("external").unwrap();
        playlists
            .add_path("external", &external.to_string_lossy())
            .unwrap();
        playlists.activate("external").unwrap();
        let playlists = Arc::new(Mutex::new(playlists));
        let (tx, mut rx) = mpsc::channel(1);
        let handle = RuntimeHandle::new(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            RuntimeShared::new(
                Arc::clone(&index),
                Arc::new(RwLock::new(Vec::new())),
                gate.clone(),
                Arc::new(Mutex::new(ContentStore::default())),
            ),
            Metrics::new(),
            config_path.clone(),
            Arc::clone(&playlists),
            directory.path().join("playlists.kdl"),
        );
        handle.ban(&hash).unwrap();

        assert_eq!(
            load_bans(&bans_path(&config_path)).unwrap(),
            HashSet::from([hash.clone()])
        );
        assert!(gate.is_banned(&hash));
        let picked = playlists
            .lock()
            .pick_from_roots(
                &[],
                &mut HashMap::new(),
                0,
                &VecDeque::new(),
                &HashSet::new(),
            )
            .unwrap();
        assert_eq!(picked, external);
        let mut applied = false;
        assert!(gate
            .run_if_allowed(&hash, || {
                applied = true;
                Ok(())
            })
            .unwrap()
            .is_none());
        assert!(!applied);

        let error = handle.set_specific(future_match).unwrap_err().to_string();
        assert!(error.contains("wallpaper is banned"), "{error}");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ban_ack_waits_for_in_flight_apply_and_blocks_later_apply() {
        let gate = BanGate::default();
        let hash = "a".repeat(64);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (attempting_tx, attempting_rx) = std::sync::mpsc::channel::<()>();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();

        std::thread::scope(|scope| {
            let apply_gate = gate.clone();
            let apply_hash = hash.clone();
            scope.spawn(move || {
                let applied = apply_gate
                    .run_if_allowed(&apply_hash, || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(())
                    })
                    .unwrap();
                assert!(applied.is_some());
            });
            entered_rx.recv().unwrap();

            let ban_gate = gate.clone();
            let ban_hash = hash.clone();
            scope.spawn(move || {
                let _update = ban_gate.0.updates.lock();
                attempting_tx.send(()).unwrap();
                ban_gate.0.hashes.write().insert(ban_hash);
                ack_tx.send(()).unwrap();
            });
            attempting_rx.recv().unwrap();
            assert!(ack_rx.try_recv().is_err());

            release_tx.send(()).unwrap();
            ack_rx.recv().unwrap();
        });

        let mut applied_after_ack = false;
        let result = gate
            .run_if_allowed(&hash, || {
                applied_after_ack = true;
                Ok(())
            })
            .unwrap();
        assert!(result.is_none());
        assert!(!applied_after_ack);
    }

    #[test]
    fn stats_reports_metric_snapshot_not_status() {
        let (tx, _rx) = mpsc::channel(4);
        let metrics = Metrics::new();
        metrics.record_swap();
        metrics.record_decode_ms(25);
        metrics.record_cache_miss();
        metrics.record_cache_hit();
        metrics.set_index_size(9);
        let handle = make_handle(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot {
                history: VecDeque::from([
                    PathBuf::from("one.jpg"),
                    PathBuf::from("two.jpg"),
                    PathBuf::from("three.jpg"),
                ]),
                ..Default::default()
            })),
            Arc::new(RwLock::new(PhotoIndex::default())),
            metrics,
        );

        let stats = handle.stats();
        assert!(stats.get("running").is_none());
        assert_eq!(stats["swaps_total"], 1);
        assert_eq!(stats["history_len"], 3);
        assert_eq!(stats["index"]["photos"], 9);
        assert_eq!(stats["decode"]["average_ms"], 25.0);
        assert_eq!(stats["cache"]["hit_ratio"], 0.5);
    }

    // -----------------------------------------------------------------------
    // test_runtime_history_bounded
    // -----------------------------------------------------------------------

    /// Pushing more than HISTORY_CAP entries into a VecDeque bounded by
    /// the cap logic should never exceed HISTORY_CAP.
    #[test]
    fn test_runtime_history_bounded() {
        let mut history: VecDeque<PathBuf> = VecDeque::new();
        for i in 0..(HISTORY_CAP + 10) {
            history.push_back(PathBuf::from(format!("img{}.jpg", i)));
            if history.len() > HISTORY_CAP {
                history.pop_front();
            }
        }
        assert_eq!(history.len(), HISTORY_CAP);
    }

    #[test]
    fn paused_runtime_blocks_automatic_swaps() {
        for reason in [
            SwapReason::Interval,
            SwapReason::AtTime,
            SwapReason::WorkspaceChange,
        ] {
            let mut pause = PauseState {
                paused: true,
                pause_until: None,
            };
            assert!(pause.blocks(&reason), "{reason:?} must be blocked");
        }
    }

    #[test]
    fn paused_runtime_allows_user_swaps() {
        let mut pause = PauseState {
            paused: true,
            pause_until: None,
        };
        assert!(!pause.blocks(&SwapReason::Manual));
        assert!(!pause.blocks(&SwapReason::Previous));
    }

    #[test]
    fn manual_swap_reports_full_queue() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(SwapRequest {
            reason: SwapReason::Interval,
            specific: None,
        })
        .unwrap();
        let handle = make_handle(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            Arc::new(RwLock::new(PhotoIndex::default())),
            Metrics::new(),
        );

        assert_eq!(
            handle.skip_next().unwrap_err().to_string(),
            "runtime swap queue is busy"
        );
    }

    // -----------------------------------------------------------------------
    // test_runtime_first_swap_no_transition
    // -----------------------------------------------------------------------

    #[test]
    fn transition_decode_requires_enabled_transition_and_previous_image() {
        assert!(!needs_transition_decode(false, true));
        assert!(!needs_transition_decode(true, false));
        assert!(needs_transition_decode(true, true));
    }

    #[test]
    fn hung_apply_helper_is_killed_at_timeout() {
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;

        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 5",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0);

        let error = wait_for_helper_child(
            command.spawn().expect("start timeout test helper"),
            Duration::from_millis(50),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("timed out after 50 milliseconds"));
    }

    // -----------------------------------------------------------------------
    // RuntimeHandle pause/resume round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_pause_resume() {
        let (tx, _rx) = mpsc::channel(4);
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot::default()));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = make_handle(tx, state, index, metrics);

        handle.pause(None);
        assert!(handle.pause_arc().lock().paused);

        handle.resume();
        assert!(!handle.pause_arc().lock().paused);
    }

    #[test]
    fn test_handle_timed_pause() {
        let (tx, _rx) = mpsc::channel(4);
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot::default()));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = make_handle(tx, state, index, metrics);

        handle.pause(Some(Duration::from_secs(60)));
        let pause_arc = handle.pause_arc();
        let p = pause_arc.lock();
        assert!(p.paused);
        assert!(p.pause_until.is_some());
    }

    #[test]
    fn huge_pause_duration_becomes_indefinite_without_panicking() {
        let (tx, _rx) = mpsc::channel(4);
        let handle = make_handle(
            tx,
            Arc::new(Mutex::new(RuntimeStateSnapshot::default())),
            Arc::new(RwLock::new(PhotoIndex::default())),
            Metrics::new(),
        );

        handle.pause(Some(Duration::MAX));

        let pause = handle.pause_arc();
        let pause = pause.lock();
        assert!(pause.paused);
        assert!(pause.pause_until.is_none());
    }

    #[test]
    fn timed_pause_expires_when_status_is_read() {
        let (tx, _rx) = mpsc::channel(4);
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot::default()));
        let handle = make_handle(
            tx,
            state,
            Arc::new(RwLock::new(PhotoIndex::default())),
            Metrics::new(),
        );

        handle.pause(Some(Duration::ZERO));
        assert_eq!(handle.status()["paused"], serde_json::json!(false));
        let pause = handle.pause_arc();
        let pause = pause.lock();
        assert!(!pause.paused);
        assert!(pause.pause_until.is_none());
    }

    // -----------------------------------------------------------------------
    // test_handle_set_folder_replaces_index
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_set_folder_replaces_index() {
        use image::{ImageBuffer, Rgb};

        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("test.jpg");
        let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(16, 16, |_, _| Rgb([255u8, 0, 0]));
        img.save(&p).unwrap();

        let (tx, _rx) = mpsc::channel(4);
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot::default()));
        // Start with an empty index.
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = make_handle(tx, state, index, Arc::clone(&metrics));

        assert_eq!(
            metrics
                .index_size
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );

        handle
            .set_folder(dir.path().to_path_buf())
            .expect("set_folder");

        // After set_folder the index should contain the one JPEG we wrote.
        assert_eq!(handle.index.read().len(), 1);
        assert_eq!(
            metrics
                .index_size
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    // -----------------------------------------------------------------------
    // test_handle_prev_returns_history_entry
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_prev_returns_history_entry() {
        let (tx, mut rx) = mpsc::channel::<SwapRequest>(4);
        let mut history: VecDeque<PathBuf> = VecDeque::new();
        history.push_back(PathBuf::from("photo_a.jpg"));
        history.push_back(PathBuf::from("photo_b.jpg"));
        history.push_back(PathBuf::from("photo_c.jpg")); // current

        let state = Arc::new(Mutex::new(RuntimeStateSnapshot {
            history,
            ..Default::default()
        }));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = make_handle(tx, state, index, metrics);

        handle
            .prev()
            .expect("prev should succeed with 3 history entries");

        let req = rx.try_recv().expect("swap channel should have a message");
        assert_eq!(req.specific, None);
        assert!(matches!(req.reason, SwapReason::Previous));
    }

    #[test]
    fn successful_previous_swaps_walk_back_without_toggling() {
        let mut history = VecDeque::from([
            PathBuf::from("photo_a.jpg"),
            PathBuf::from("photo_b.jpg"),
            PathBuf::from("photo_c.jpg"),
        ]);

        for expected in ["photo_b.jpg", "photo_a.jpg"] {
            let path = previous_path(&history).unwrap();
            assert_eq!(path, PathBuf::from(expected));
            record_successful_history(&mut history, &path, &SwapReason::Previous);
            assert_eq!(history.back(), Some(&path));
        }
        assert!(previous_path(&history).is_none());
    }

    // -----------------------------------------------------------------------
    // test_handle_prev_fails_with_no_history
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_prev_fails_with_no_history() {
        let (tx, _rx) = mpsc::channel::<SwapRequest>(4);
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot::default()));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = make_handle(tx, state, index, metrics);

        let result = handle.prev();
        assert!(result.is_err(), "prev on empty history should return Err");
    }

    #[test]
    fn test_handle_set_specific_rejects_missing_file() {
        let (tx, mut rx) = mpsc::channel(4);
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot::default()));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let handle = make_handle(tx, state, index, Metrics::new());

        assert!(handle
            .set_specific(PathBuf::from("does-not-exist.jpg"))
            .is_err());
        assert!(
            rx.try_recv().is_err(),
            "invalid path must not enqueue a swap"
        );
    }
}

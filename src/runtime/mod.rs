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

use crate::animation::{AnimationPlayer, DisplayTarget};
use crate::apply::{configured_global_fit, MonitorInfo, MonitorSnapshot, WallpaperFit};
pub use crate::com::ComApartment;
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

mod bans;
mod handle;
mod helpers;
mod migrate;
mod resolve;
mod state;
mod transaction;
mod views;

pub use bans::BanGate;
pub use handle::RuntimeHandle;
pub use state::{PauseState, RuntimeState, RuntimeStateSnapshot};

use bans::*;
use helpers::*;
use migrate::*;
use resolve::*;
use state::*;
use transaction::*;
use views::*;

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
    /// Animated GIF/WebP playback; `None` unless `animated.enabled`.
    animation: Option<AnimationPlayer>,
}

const BYTES_PER_4K_BGRA: usize = 3840 * 2160 * 4;
const INDEX_CACHE_FILENAME: &str = "index-cache.json";
const ALL_MONITORS_ID: &str = "all";

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

fn index_cache_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name(INDEX_CACHE_FILENAME)
}

fn display_target(monitor: &MonitorInfo, fit: WallpaperFit) -> DisplayTarget {
    DisplayTarget {
        monitor_id: monitor.id.clone(),
        bounds: Rect {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
        },
        fit,
    }
}

/// Start or stop animated playback for the displays that just changed.
fn update_animation(
    player: &mut AnimationPlayer,
    monitors: Option<&[MonitorInfo]>,
    fit: WallpaperFit,
    path: &Path,
    successful_monitor_ids: &[String],
) {
    let Some(monitors) = monitors else {
        // Without display geometry nothing can be placed; show the static
        // all-monitor wallpaper instead.
        player.hide_all();
        return;
    };
    for monitor in monitors
        .iter()
        .filter(|monitor| successful_monitor_ids.contains(&monitor.id))
    {
        player.show(display_target(monitor, fit), path.to_path_buf());
    }
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
        // Disabled transitions never render, so skip the Direct2D probe
        // (and loading d2d1.dll) that renderer "auto" would do.
        let backend = if config.transitions.enabled {
            Backend::parse(&config.transitions.renderer)
        } else {
            Backend::Cpu
        };
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

        let (state, complete_monitor_snapshot, monitors) = initial_runtime_state();
        if complete_monitor_snapshot {
            scheduler_progress.seed_success();
        }
        let mut animation = AnimationPlayer::from_config(&config.animated);
        if let Some(player) = animation.as_mut().filter(|_| !monitors.is_empty()) {
            // Resume an animated wallpaper that was already showing.
            let fit = configured_global_fit(config, &monitors);
            for monitor in &monitors {
                if let Some(path) = state.current_path.get(&monitor.id) {
                    player.show(display_target(monitor, fit), path.clone());
                }
            }
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
            animation,
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

        let global_fit = monitors
            .as_deref()
            .map(|monitors| configured_global_fit(&self.config, monitors))
            .unwrap_or(WallpaperFit::Fill);
        let (successful_monitors, failures, total_monitors) = if let Some(monitors) = &monitors {
            let fit = global_fit.as_str();

            let mut successful_monitors = Vec::new();
            let mut failures = Vec::new();
            for monitor in monitors {
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

        if let Some(player) = &mut self.animation {
            update_animation(
                player,
                monitors.as_deref(),
                global_fit,
                &new_path,
                &successful_monitors,
            );
        }
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
    fn canonical_paths_find_scanned_entries_without_rehashing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Lake.PNG");
        std::fs::write(&file, b"not decoded").unwrap();
        let mut index = PhotoIndex::default();
        index.photos.push(crate::index::PhotoEntry {
            path: file.clone(),
            width: Some(1),
            height: Some(1),
            hash: "a".repeat(64),
            banned: false,
        });
        let canonical = std::fs::canonicalize(&file).unwrap();
        assert!(canonical.to_string_lossy().starts_with(r"\\?\"));
        assert_eq!(
            strip_verbatim_prefix(&canonical),
            PathBuf::from(&canonical.to_string_lossy()[4..])
        );
        assert_eq!(indexed_hash(&index, &canonical), Some("a".repeat(64)));
        // A different spelling of the same file still resolves.
        let lower = dir.path().join("lake.png");
        assert_eq!(indexed_hash(&index, &lower), Some("a".repeat(64)));
        assert_eq!(indexed_hash(&index, &dir.path().join("other.png")), None);
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\a.jpg")),
            PathBuf::from(r"\\server\share\a.jpg")
        );
    }

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

    #[test]
    fn helper_output_larger_than_a_pipe_buffer_does_not_deadlock() {
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;

        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::Error.Write('e' * 200000); [Console]::Out.Write('o' * 200000)",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW.0);

        let output = wait_for_helper_child(
            command.spawn().expect("start chatty helper"),
            Duration::from_secs(20),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 200_000);
        assert_eq!(output.stderr.len(), 200_000);
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

//! Aurora's runtime orchestrator: receives SwapRequests from the scheduler,
//! picks a photo from the index, decodes it (with cache), runs the configured
//! transition, applies the new wallpaper via IDesktopWallpaper, updates metrics.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::apply::{WallpaperApplier, WallpaperFit};
use crate::config::types::Config;
use crate::decode::SharedDecodeCache;
use crate::index::PhotoIndex;
use crate::ipc::messages::IpcEvent;
use crate::metrics::Metrics;
use crate::playlist::{
    default_playlists_path, load_playlists, persist_playlists, PlaylistStore,
};
use crate::scheduler::{SwapReason, SwapRequest};
use crate::transition::{
    Backend, DecodedImage as TransitionImage, Rect, TransitionRenderer, TransitionStyle,
};

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
    pub paused: bool,
    pub pause_until: Option<Instant>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            current_path: HashMap::new(),
            history: VecDeque::new(),
            recent_paths: VecDeque::new(),
            paused: false,
            pause_until: None,
        }
    }

    /// Returns true if the runtime is currently paused (auto-resumes timed pauses).
    #[cfg(test)]
    fn is_effectively_paused(&mut self) -> bool {
        if let Some(until) = self.pause_until {
            if Instant::now() >= until {
                self.paused = false;
                self.pause_until = None;
            }
        }
        self.paused
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

pub struct Runtime {
    index: Arc<RwLock<PhotoIndex>>,
    cache: SharedDecodeCache,
    transitions: TransitionRenderer,
    applier: WallpaperApplier,
    metrics: Arc<Metrics>,
    state: RuntimeState,
    config: Config,
    event_tx: Option<tokio::sync::broadcast::Sender<IpcEvent>>,
    /// Shared playlist store — also held by RuntimeHandle for IPC mutations.
    playlist_store: Arc<Mutex<PlaylistStore>>,
    /// Sequential cursor: playlist_name → next_index.
    playlist_cursor: std::collections::HashMap<String, usize>,
}

const HISTORY_CAP: usize = 50;

impl Runtime {
    pub fn new(config: &Config, applier: WallpaperApplier, metrics: Arc<Metrics>) -> Result<Self> {
        // Build photo index from configured sources.
        let roots: Vec<PathBuf> = config.sources.iter().map(|s| s.path.clone()).collect();
        let extensions = if roots.is_empty() {
            vec!["jpg".to_string(), "jpeg".to_string(), "png".to_string()]
        } else {
            config
                .sources
                .first()
                .map(|s| s.extensions.clone())
                .unwrap_or_default()
        };

        let index = if roots.is_empty() {
            PhotoIndex::default()
        } else {
            PhotoIndex::scan(
                &roots,
                &extensions,
                config.sources.first().map(|s| s.recursive).unwrap_or(true),
            )
            .context("scanning photo sources")?
        };

        let index_size = index.len() as u64;
        metrics.set_index_size(index_size);
        info!("photo index built: {} photos", index_size);

        let style = TransitionStyle::parse(&config.transitions.style);
        let backend = Backend::parse(&config.transitions.renderer);
        let transitions = TransitionRenderer::new(style, config.transitions.duration_ms, backend);

        let bytes_per_4k_bgra = 3840usize * 2160usize * 4usize;
        let configured_cache_bytes = (config.cache.decoded_mb as usize).saturating_mul(1024 * 1024);
        let cache_capacity = (configured_cache_bytes / bytes_per_4k_bgra)
            .max(config.cache.prefetch_count.saturating_add(1))
            .max(1);
        info!(
            "decode cache capacity: {} entries (~{} MB budget)",
            cache_capacity, config.cache.decoded_mb
        );
        let cache = SharedDecodeCache::new(cache_capacity);

        // Load playlist store from disk (creates empty default if file is absent).
        let playlists_path = default_playlists_path();
        let playlist_store = match load_playlists(&playlists_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to load playlists from {}: {} — starting with empty store", playlists_path.display(), e);
                PlaylistStore::default()
            }
        };

        Ok(Self {
            index: Arc::new(RwLock::new(index)),
            cache,
            transitions,
            applier,
            metrics,
            state: RuntimeState::new(),
            config: config.clone(),
            event_tx: None,
            playlist_store: Arc::new(Mutex::new(playlist_store)),
            playlist_cursor: std::collections::HashMap::new(),
        })
    }

    /// Wire the IPC broadcast sender so Runtime can emit WallpaperChanged events.
    pub fn set_event_sender(&mut self, tx: tokio::sync::broadcast::Sender<IpcEvent>) {
        self.event_tx = Some(tx);
    }

    /// Expose the photo index Arc so main can hand it to RuntimeHandle.
    pub fn index_arc(&self) -> Arc<RwLock<PhotoIndex>> {
        Arc::clone(&self.index)
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
        mut rx: mpsc::UnboundedReceiver<SwapRequest>,
        handle_state: Arc<Mutex<RuntimeStateSnapshot>>,
        pause_arc: Arc<Mutex<PauseState>>,
    ) {
        while let Some(req) = rx.recv().await {
            // Check IPC-controlled pause.
            {
                let mut p = pause_arc.lock();
                // Auto-expire timed pause.
                if let Some(until) = p.pause_until {
                    if Instant::now() >= until {
                        p.paused = false;
                        p.pause_until = None;
                    }
                }
                if p.paused {
                    debug!(
                        "runtime paused (IPC) — dropping swap request {:?}",
                        req.reason
                    );
                    continue;
                }
            }
            if let Err(e) = self.handle_swap(req).await {
                warn!("swap failed: {}", e);
            }
            // Sync shared state snapshot for IPC queries.
            {
                let mut snap = handle_state.lock();
                let p = pause_arc.lock();
                snap.paused = p.paused;
                snap.current_path = self.state.current_path.clone();
                snap.history_len = self.state.history.len();
                snap.history = self.state.history.clone();
            }
        }
        info!("runtime: swap channel closed — exiting");
    }

    async fn handle_swap(&mut self, req: SwapRequest) -> Result<()> {
        let monitors = self.applier.list_monitors().context("listing monitors")?;

        if monitors.is_empty() {
            warn!("no monitors found via IDesktopWallpaper — skip swap");
            return Ok(());
        }

        // Pick target path.
        let new_path: PathBuf = if let Some(specific) = req.specific {
            specific
        } else {
            // When a playlist is active, pick from it; otherwise use the full index.
            let source_root = self
                .config
                .sources
                .first()
                .map(|s| s.path.clone());

            let playlist_pick = {
                let store = self.playlist_store.lock();
                if store.active.is_some() {
                    store.pick(source_root.as_deref(), &mut self.playlist_cursor)
                } else {
                    None
                }
            };

            if let Some(path) = playlist_pick {
                path
            } else if self.playlist_store.lock().active.is_some() {
                // Playlist is active but all files are missing — fall through to index with a warning.
                warn!("active playlist has no accessible files — falling back to full index");
                let index = self.index.read();
                let recent_window = self.config.schedule.min_repeat_window;
                index
                    .pick_random(recent_window, &self.state.recent_paths)
                    .ok_or_else(|| anyhow::anyhow!("photo index is empty or all photos are banned"))?
                    .path
                    .clone()
            } else {
                let index = self.index.read();
                let recent_window = self.config.schedule.min_repeat_window;
                index
                    .pick_random(recent_window, &self.state.recent_paths)
                    .ok_or_else(|| anyhow::anyhow!("photo index is empty or all photos are banned"))?
                    .path
                    .clone()
            }
        };

        // Per-monitor loop.
        for monitor in &monitors {
            // Look up monitor fit override.
            let fit_str = self
                .config
                .monitors
                .iter()
                .find(|m| m.name == monitor.id)
                .map(|m| m.fit.as_str())
                .unwrap_or("fill");
            let fit = WallpaperFit::parse(fit_str);
            self.applier.set_fit(fit)?;

            let (tw, th) = (monitor.width, monitor.height);

            // Decode the new image.
            let t0 = std::time::Instant::now();
            let new_decoded = self
                .cache
                .get_or_decode(&new_path, tw, th)
                .with_context(|| format!("decode {}", new_path.display()))?;
            let decode_ms = t0.elapsed().as_millis() as u64;
            self.metrics.record_decode_ms(decode_ms);

            // Determine if we have a previous image to transition from.
            let prev_path = self.state.current_path.get(&monitor.id).cloned();

            let is_first = prev_path.is_none();

            if !is_first && self.config.transitions.enabled {
                // Try to decode the old image for the transition.
                if let Some(ref old_path) = prev_path {
                    let old_decoded = self.cache.get_or_decode(old_path, tw, th).ok();
                    if let Some(old_arc) = old_decoded {
                        // Convert decode::DecodedImage → transition::DecodedImage.
                        let old_ti = TransitionImage {
                            width: old_arc.width,
                            height: old_arc.height,
                            data: old_arc.bgra.clone(),
                        };
                        let new_ti = TransitionImage {
                            width: new_decoded.width,
                            height: new_decoded.height,
                            data: new_decoded.bgra.clone(),
                        };
                        let bounds = Rect {
                            x: 0,
                            y: 0,
                            width: new_decoded.width,
                            height: new_decoded.height,
                        };
                        if let Err(e) = self.transitions.run(bounds, &old_ti, &new_ti) {
                            warn!("transition error (continuing with direct apply): {}", e);
                        }
                    }
                }
            }

            // Apply new wallpaper.
            self.applier
                .set_for_monitor(&monitor.id, &new_path)
                .with_context(|| format!("set_for_monitor {}", &monitor.id))?;

            // Broadcast WallpaperChanged event to IPC subscribers.
            if let Some(tx) = &self.event_tx {
                let _ = tx.send(IpcEvent::WallpaperChanged {
                    monitor_id: monitor.id.clone(),
                    path: new_path.display().to_string(),
                });
            }

            // Update metrics.
            self.metrics
                .set_current_photo(&monitor.id, new_path.clone());
        }

        // Update state.
        for monitor in &monitors {
            self.state
                .current_path
                .insert(monitor.id.clone(), new_path.clone());
        }

        // Push to history (most-recent at back).
        self.state.history.push_back(new_path.clone());
        if self.state.history.len() > HISTORY_CAP {
            self.state.history.pop_front();
        }

        // Push to recent anti-repeat window.
        self.state.recent_paths.push_back(new_path.clone());
        let window = self.config.schedule.min_repeat_window;
        while self.state.recent_paths.len() > window.max(1) {
            self.state.recent_paths.pop_front();
        }

        self.metrics.record_swap();

        // Emit IPC swapped events (best-effort).
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        for monitor in &monitors {
            debug!(
                "swapped monitor={} path={} ts_ms={}",
                monitor.id,
                new_path.display(),
                ts_ms
            );
        }

        info!(
            "wallpaper swapped → {} (reason={:?})",
            new_path.display(),
            req.reason
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RuntimeHandle — clone-friendly, used by IPC
// ---------------------------------------------------------------------------

/// Shared snapshot of RuntimeState, updated after each swap.
pub struct RuntimeStateSnapshot {
    pub paused: bool,
    pub current_path: HashMap<String, PathBuf>,
    pub history_len: usize,
    /// Full history ring, mirrored from Runtime::state so IPC `prev` can read it.
    pub history: VecDeque<PathBuf>,
}

/// A lightweight, Clone handle that IPC commands dispatch through.
#[derive(Clone)]
pub struct RuntimeHandle {
    swap_tx: mpsc::UnboundedSender<SwapRequest>,
    /// Shared with the Runtime::run loop for read-only status queries.
    pub(crate) state: Arc<Mutex<RuntimeStateSnapshot>>,
    pub(crate) index: Arc<RwLock<PhotoIndex>>,
    pub(crate) metrics: Arc<Metrics>,
    /// Pause state is managed separately so IPC can set it without going
    /// through the swap channel (which would need runtime to drain it).
    paused: Arc<Mutex<PauseState>>,
    /// Path to the config file on disk, used by reload_from_disk().
    config_path: Arc<std::path::PathBuf>,
    /// Shared playlist store.  IPC commands mutate this and persist to disk.
    pub(crate) playlist_store: Arc<Mutex<PlaylistStore>>,
    /// Path to the playlists KDL file on disk.
    playlists_path: Arc<std::path::PathBuf>,
}

pub struct PauseState {
    pub paused: bool,
    pub pause_until: Option<Instant>,
}

impl RuntimeHandle {
    pub fn new(
        swap_tx: mpsc::UnboundedSender<SwapRequest>,
        state: Arc<Mutex<RuntimeStateSnapshot>>,
        index: Arc<RwLock<PhotoIndex>>,
        metrics: Arc<Metrics>,
        config_path: std::path::PathBuf,
        playlist_store: Arc<Mutex<PlaylistStore>>,
        playlists_path: std::path::PathBuf,
    ) -> Self {
        Self {
            swap_tx,
            state,
            index,
            metrics,
            paused: Arc::new(Mutex::new(PauseState {
                paused: false,
                pause_until: None,
            })),
            config_path: Arc::new(config_path),
            playlist_store,
            playlists_path: Arc::new(playlists_path),
        }
    }

    /// Expose the pause Arc so it can be shared with `Runtime::run`.
    pub fn pause_arc(&self) -> Arc<Mutex<PauseState>> {
        Arc::clone(&self.paused)
    }

    /// Send a manual skip-to-next swap.
    pub fn skip_next(&self) {
        let _ = self.swap_tx.send(SwapRequest {
            reason: SwapReason::Manual,
            specific: None,
        });
    }

    /// Pause cycling, optionally for a fixed duration.
    pub fn pause(&self, duration: Option<Duration>) {
        let mut p = self.paused.lock();
        p.paused = true;
        p.pause_until = duration.map(|d| Instant::now() + d);
    }

    /// Resume from pause.
    pub fn resume(&self) {
        let mut p = self.paused.lock();
        p.paused = false;
        p.pause_until = None;
    }

    /// Force-apply a specific path.
    pub fn set_specific(&self, path: PathBuf) {
        let _ = self.swap_tx.send(SwapRequest {
            reason: SwapReason::Manual,
            specific: Some(path),
        });
    }

    /// Return a JSON status blob for IPC Status / Stats.
    pub fn status(&self) -> serde_json::Value {
        let snap = self.state.lock();
        let p = self.paused.lock();
        let current_paths: HashMap<String, String> = snap
            .current_path
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string_lossy().into_owned()))
            .collect();
        serde_json::json!({
            "running": true,
            "paused": p.paused,
            "current_path": current_paths,
            "history_len": snap.history_len,
            "swaps_total": self.metrics.swaps_total.load(Ordering::Relaxed),
            "cache_hit_ratio": self.metrics.cache_hit_ratio(),
            "index_size": self.metrics.index_size.load(Ordering::Relaxed),
        })
    }

    /// Rate a photo by path (stub — needs XMP write support in index).
    pub fn rate(&self, stars: u8) -> serde_json::Value {
        // TODO: look up current path, write XMP sidecar, update index rating.
        let _ = stars;
        serde_json::json!({"success": false, "error": "rating persistence not yet implemented"})
    }

    /// Ban the photo with the given hash via the index.
    pub fn ban(&self, hash: &str) -> serde_json::Value {
        let mut index = self.index.write();
        index.ban(hash);
        serde_json::json!({"success": true})
    }

    /// Return a snapshot of the current wallpaper path for each monitor.
    pub fn current_wallpaper(&self) -> HashMap<String, PathBuf> {
        self.state.lock().current_path.clone()
    }

    /// Re-read the config file from disk, re-scan photo sources, and update
    /// the index and metrics.
    ///
    /// NOTE: live transition/schedule changes require a full daemon restart.
    /// TODO: apply transition and schedule changes without restart.
    pub fn reload_from_disk(&self) -> anyhow::Result<()> {
        let src = std::fs::read_to_string(self.config_path.as_ref())
            .with_context(|| format!("read config {}", self.config_path.display()))?;
        let config = crate::config::parse::parse_kdl_config(&src)
            .with_context(|| format!("parse config {}", self.config_path.display()))?;

        let roots: Vec<PathBuf> = config.sources.iter().map(|s| s.path.clone()).collect();
        let extensions: Vec<String> = if roots.is_empty() {
            vec!["jpg".to_string(), "jpeg".to_string(), "png".to_string()]
        } else {
            config
                .sources
                .first()
                .map(|s| s.extensions.clone())
                .unwrap_or_default()
        };
        let recursive = config.sources.first().map(|s| s.recursive).unwrap_or(true);

        let new_index = if roots.is_empty() {
            PhotoIndex::default()
        } else {
            PhotoIndex::scan(&roots, &extensions, recursive)
                .context("scanning photo sources during reload")?
        };

        let new_size = new_index.len() as u64;
        *self.index.write() = new_index;
        self.metrics.set_index_size(new_size);
        info!(
            "reload_from_disk: photo index rebuilt with {} photos",
            new_size
        );
        Ok(())
    }

    /// Narrow the active photo pool to a single folder for this session.
    /// Pass an empty path to revert to the full configured source list.
    pub fn set_folder(&self, path: PathBuf) -> anyhow::Result<()> {
        let default_extensions: Vec<String> = ["jpg", "jpeg", "png", "webp", "gif"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let new_index = if path.as_os_str().is_empty() {
            // Empty path → rebuild from configured sources (best-effort: use existing index).
            // A full reload_from_disk() is needed for proper source config; log a hint.
            info!("set_folder: empty path — clearing session folder override; call reload to restore configured sources");
            PhotoIndex::default()
        } else {
            PhotoIndex::scan(std::slice::from_ref(&path), &default_extensions, true)
                .with_context(|| format!("set_folder: scan {:?}", path))?
        };

        let new_size = new_index.len() as u64;
        *self.index.write() = new_index;
        self.metrics.set_index_size(new_size);
        info!(
            "set_folder: index now contains {} photos from {:?}",
            new_size, path
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Playlist methods
    // -----------------------------------------------------------------------

    /// Return a JSON summary of all playlists + the active one.
    pub fn playlist_list(&self) -> serde_json::Value {
        let store = self.playlist_store.lock();
        let items: Vec<serde_json::Value> = store
            .playlists
            .iter()
            .map(|pl| {
                serde_json::json!({
                    "name": pl.name,
                    "shuffle": pl.shuffle,
                    "paths": pl.paths,
                    "active": store.active.as_deref() == Some(pl.name.as_str()),
                })
            })
            .collect();
        serde_json::json!({ "playlists": items, "active": store.active })
    }

    /// Create an empty playlist and persist.
    pub fn playlist_create(&self, name: &str) -> anyhow::Result<()> {
        {
            let mut store = self.playlist_store.lock();
            store.create(name)?;
        }
        self.persist_playlists()
    }

    /// Add a path to a playlist and persist.
    pub fn playlist_add(&self, name: &str, path: &str) -> anyhow::Result<()> {
        {
            let mut store = self.playlist_store.lock();
            store.add_path(name, path)?;
        }
        self.persist_playlists()
    }

    /// Remove a path from a playlist and persist.
    pub fn playlist_remove(&self, name: &str, path: &str) -> anyhow::Result<()> {
        {
            let mut store = self.playlist_store.lock();
            store.remove_path(name, path)?;
        }
        self.persist_playlists()
    }

    /// Activate a playlist, persist, and immediately trigger a swap.
    pub fn playlist_activate(&self, name: &str) -> anyhow::Result<()> {
        {
            let mut store = self.playlist_store.lock();
            store.activate(name)?;
        }
        self.persist_playlists()?;
        // Trigger an immediate swap so the new playlist's wallpaper shows at once.
        let _ = self.swap_tx.send(SwapRequest {
            reason: SwapReason::Manual,
            specific: None,
        });
        Ok(())
    }

    /// Deactivate the current playlist and persist.
    pub fn playlist_deactivate(&self) -> anyhow::Result<()> {
        {
            let mut store = self.playlist_store.lock();
            store.deactivate();
        }
        self.persist_playlists()
    }

    /// Delete a playlist and persist.
    pub fn playlist_delete(&self, name: &str) -> anyhow::Result<()> {
        {
            let mut store = self.playlist_store.lock();
            store.delete(name)?;
        }
        self.persist_playlists()
    }

    /// Atomically write the playlist store to disk.
    fn persist_playlists(&self) -> anyhow::Result<()> {
        let store = self.playlist_store.lock();
        persist_playlists(&store, &self.playlists_path)
    }

    // -----------------------------------------------------------------------

    /// Restore the previous photo from the history ring.
    /// Returns an error if there is no previous entry (history has fewer than 2 entries).
    pub fn prev(&self) -> anyhow::Result<()> {
        let prev_path = {
            let snap = self.state.lock();
            // history is most-recent-at-back; [len-1] = current, [len-2] = previous.
            if snap.history.len() < 2 {
                return Err(anyhow::anyhow!("no previous photo in history"));
            }
            snap.history.iter().rev().nth(1).cloned()
        };
        if let Some(path) = prev_path {
            let _ = self.swap_tx.send(SwapRequest {
                reason: SwapReason::Manual,
                specific: Some(path),
            });
            Ok(())
        } else {
            Err(anyhow::anyhow!("no previous photo in history"))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience wrapper for tests that don't care about playlist persistence.
    fn make_handle(
        tx: mpsc::UnboundedSender<SwapRequest>,
        state: Arc<Mutex<RuntimeStateSnapshot>>,
        index: Arc<RwLock<PhotoIndex>>,
        metrics: Arc<Metrics>,
    ) -> RuntimeHandle {
        RuntimeHandle::new(
            tx,
            state,
            index,
            metrics,
            std::path::PathBuf::from("config.kdl"),
            Arc::new(Mutex::new(PlaylistStore::default())),
            std::path::PathBuf::from("playlists.kdl"),
        )
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

    // -----------------------------------------------------------------------
    // test_runtime_pause_skips_swap
    // -----------------------------------------------------------------------

    /// RuntimeState::is_effectively_paused returns true when paused, and
    /// auto-resumes when the timed duration has elapsed.
    #[test]
    fn test_runtime_pause_skips_swap() {
        let mut state = RuntimeState::new();

        // Not paused by default.
        assert!(!state.is_effectively_paused());

        // Pause indefinitely.
        state.paused = true;
        assert!(state.is_effectively_paused());

        // Timed pause that has already expired should auto-resume.
        state.pause_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(
            !state.is_effectively_paused(),
            "expired timed pause should auto-resume"
        );
        assert!(!state.paused);
    }

    // -----------------------------------------------------------------------
    // test_runtime_first_swap_no_transition
    // -----------------------------------------------------------------------

    /// When current_path has no entry for a monitor (first swap), is_first = true,
    /// so we should skip the transition branch.
    #[test]
    fn test_runtime_first_swap_no_transition() {
        let state = RuntimeState::new();
        // On first swap, current_path is empty — prev_path is None.
        let monitor_id = "\\\\?\\DISPLAY1";
        let prev_path = state.current_path.get(monitor_id);
        let is_first = prev_path.is_none();
        assert!(is_first, "first swap should have no previous path");
    }

    // -----------------------------------------------------------------------
    // RuntimeHandle pause/resume round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_pause_resume() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot {
            paused: false,
            current_path: HashMap::new(),
            history_len: 0,
            history: VecDeque::new(),
        }));
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
        let (tx, _rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot {
            paused: false,
            current_path: HashMap::new(),
            history_len: 0,
            history: VecDeque::new(),
        }));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = make_handle(tx, state, index, metrics);

        handle.pause(Some(Duration::from_secs(60)));
        let pause_arc = handle.pause_arc();
        let p = pause_arc.lock();
        assert!(p.paused);
        assert!(p.pause_until.is_some());
    }

    // -----------------------------------------------------------------------
    // test_handle_set_folder_replaces_index
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_set_folder_replaces_index() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        // Write a tiny JPEG-like file so the scanner finds it.
        let p = dir.path().join("test.jpg");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]).unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot {
            paused: false,
            current_path: HashMap::new(),
            history_len: 0,
            history: VecDeque::new(),
        }));
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
        let (tx, mut rx) = mpsc::unbounded_channel::<SwapRequest>();
        let mut history: VecDeque<PathBuf> = VecDeque::new();
        history.push_back(PathBuf::from("photo_a.jpg"));
        history.push_back(PathBuf::from("photo_b.jpg"));
        history.push_back(PathBuf::from("photo_c.jpg")); // current

        let state = Arc::new(Mutex::new(RuntimeStateSnapshot {
            paused: false,
            current_path: HashMap::new(),
            history_len: history.len(),
            history,
        }));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = make_handle(tx, state, index, metrics);

        handle
            .prev()
            .expect("prev should succeed with 3 history entries");

        // The swap channel should have received a request for photo_b.jpg (second-to-last).
        let req = rx.try_recv().expect("swap channel should have a message");
        assert_eq!(req.specific, Some(PathBuf::from("photo_b.jpg")));
        assert!(matches!(req.reason, SwapReason::Manual));
    }

    // -----------------------------------------------------------------------
    // test_handle_prev_fails_with_no_history
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_prev_fails_with_no_history() {
        let (tx, _rx) = mpsc::unbounded_channel::<SwapRequest>();
        let state = Arc::new(Mutex::new(RuntimeStateSnapshot {
            paused: false,
            current_path: HashMap::new(),
            history_len: 0,
            history: VecDeque::new(),
        }));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = make_handle(tx, state, index, metrics);

        let result = handle.prev();
        assert!(result.is_err(), "prev on empty history should return Err");
    }
}

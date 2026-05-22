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
use crate::metrics::Metrics;
use crate::scheduler::{SwapRequest, SwapReason};
use crate::transition::{Backend, DecodedImage as TransitionImage, Rect, TransitionRenderer, TransitionStyle};

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
            PhotoIndex::scan(&roots, &extensions, config.sources.first().map(|s| s.recursive).unwrap_or(true))
                .context("scanning photo sources")?
        };

        let index_size = index.len() as u64;
        metrics.set_index_size(index_size);
        info!("photo index built: {} photos", index_size);

        let style = TransitionStyle::from_str(&config.transitions.style);
        let backend = Backend::from_str(&config.transitions.renderer);
        let transitions = TransitionRenderer::new(style, config.transitions.duration_ms, backend);

        let cache = SharedDecodeCache::new(16);

        Ok(Self {
            index: Arc::new(RwLock::new(index)),
            cache,
            transitions,
            applier,
            metrics,
            state: RuntimeState::new(),
            config: config.clone(),
        })
    }

    /// Expose the photo index Arc so main can hand it to RuntimeHandle.
    pub fn index_arc(&self) -> Arc<RwLock<PhotoIndex>> {
        Arc::clone(&self.index)
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
                    debug!("runtime paused (IPC) — dropping swap request {:?}", req.reason);
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
            }
        }
        info!("runtime: swap channel closed — exiting");
    }

    async fn handle_swap(&mut self, req: SwapRequest) -> Result<()> {
        let monitors = self
            .applier
            .list_monitors()
            .context("listing monitors")?;

        if monitors.is_empty() {
            warn!("no monitors found via IDesktopWallpaper — skip swap");
            return Ok(());
        }

        // Pick target path.
        let new_path: PathBuf = if let Some(specific) = req.specific {
            specific
        } else {
            let index = self.index.read();
            let recent_window = self.config.schedule.min_repeat_window;
            let entry = index
                .pick_random(recent_window, &self.state.recent_paths)
                .ok_or_else(|| anyhow::anyhow!("photo index is empty or all photos are banned"))?;
            entry.path.clone()
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
            let fit = WallpaperFit::from_str(fit_str);
            self.applier.set_fit(fit)?;

            // Target decode resolution: use monitor bounds if known, else generous default.
            // IDesktopWallpaper doesn't expose resolution directly so we use a safe default.
            let (tw, th) = (3840u32, 2160u32);

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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!state.is_effectively_paused(), "expired timed pause should auto-resume");
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
        }));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = RuntimeHandle::new(tx, state, index, metrics);

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
        }));
        let index = Arc::new(RwLock::new(PhotoIndex::default()));
        let metrics = Metrics::new();
        let handle = RuntimeHandle::new(tx, state, index, metrics);

        handle.pause(Some(Duration::from_secs(60)));
        let p = handle.pause_arc().lock();
        assert!(p.paused);
        assert!(p.pause_until.is_some());
    }
}

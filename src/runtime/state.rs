//! Per-monitor wallpaper state, history, and pause state.

use super::*;

pub struct RuntimeState {
    /// Current wallpaper path per monitor ID.
    pub current_path: HashMap<String, PathBuf>,
    /// History ring for `aurora-ctl prev` (most-recent at back).
    pub history: VecDeque<PathBuf>,
    /// Recent paths for anti-repeat window.
    pub recent_paths: VecDeque<PathBuf>,
}

impl RuntimeState {
    pub(super) fn new() -> Self {
        Self {
            current_path: HashMap::new(),
            history: VecDeque::new(),
            recent_paths: VecDeque::new(),
        }
    }
}

pub(super) const HISTORY_CAP: usize = 50;

pub(super) fn commit_successful_monitors(
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

pub(super) fn should_suppress_same_target(
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

pub(super) fn reconcile_attached_monitors(
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

pub(super) fn previous_path(history: &VecDeque<PathBuf>) -> Option<PathBuf> {
    history.iter().rev().nth(1).cloned()
}

pub(super) fn record_successful_history(
    history: &mut VecDeque<PathBuf>,
    path: &Path,
    reason: &SwapReason,
) {
    if *reason == SwapReason::Previous {
        history.pop_back();
    } else {
        history.push_back(path.to_path_buf());
        if history.len() > HISTORY_CAP {
            history.pop_front();
        }
    }
}

pub(super) fn seed_recent_current_paths(state: &mut RuntimeState) {
    let mut seen = HashSet::new();
    for path in state.current_path.values() {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if seen.insert(path.clone()) {
            state.recent_paths.push_back(path);
        }
    }
}

pub(super) fn runtime_state_from_snapshots(
    snapshots: Vec<MonitorSnapshot>,
) -> (RuntimeState, bool) {
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

pub(super) fn initial_runtime_state() -> (RuntimeState, bool, Vec<MonitorInfo>) {
    match inspect_wallpapers_in_child() {
        Ok(snapshots) => {
            let monitors = snapshots
                .iter()
                .map(|snapshot| snapshot.monitor.clone())
                .collect();
            let (state, complete) = runtime_state_from_snapshots(snapshots);
            (state, complete, monitors)
        }
        Err(error) => {
            warn!("could not seed current wallpapers: {error:#}");
            (RuntimeState::new(), false, Vec::new())
        }
    }
}

/// Shared snapshot of RuntimeState, updated after each swap.
#[derive(Default)]
pub struct RuntimeStateSnapshot {
    pub current_path: HashMap<String, PathBuf>,
    /// Full history ring, mirrored from Runtime::state so IPC `prev` can read it.
    pub history: VecDeque<PathBuf>,
}

pub struct PauseState {
    pub paused: bool,
    pub pause_until: Option<Instant>,
}

impl PauseState {
    pub(super) fn is_paused(&mut self) -> bool {
        if self
            .pause_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.paused = false;
            self.pause_until = None;
        }
        self.paused
    }

    pub(super) fn blocks(&mut self, reason: &SwapReason) -> bool {
        self.is_paused() && !matches!(reason, SwapReason::Manual | SwapReason::Previous)
    }
}

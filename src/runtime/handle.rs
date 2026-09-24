//! `RuntimeHandle`: the clonable IPC-facing side of the runtime.

use super::*;

/// A lightweight, Clone handle that IPC commands dispatch through.
#[derive(Clone)]
pub struct RuntimeHandle {
    pub(super) swap_tx: mpsc::Sender<SwapRequest>,
    /// Shared with the Runtime::run loop for read-only status queries.
    pub(crate) state: Arc<Mutex<RuntimeStateSnapshot>>,
    pub(crate) index: Arc<RwLock<PhotoIndex>>,
    /// Effective roots used to resolve relative playlist entries.
    pub(super) source_roots: Arc<RwLock<Vec<PathBuf>>>,
    pub(crate) metrics: Arc<Metrics>,
    /// Pause state is managed separately so IPC can set it without going
    /// through the swap channel (which would need runtime to drain it).
    pub(super) paused: Arc<Mutex<PauseState>>,
    /// Path to the config file on disk, used by reload_from_disk().
    pub(super) config_path: Arc<std::path::PathBuf>,
    /// Serializes source scans without delaying unrelated ban updates.
    pub(super) source_updates: Arc<Mutex<()>>,
    /// Serializes playlist/content recovery and persistence.
    pub(super) persistence: Arc<Mutex<()>>,
    /// Serializes ban persistence and gates the final wallpaper apply.
    pub(super) ban_gate: BanGate,
    /// Shared playlist store.  IPC commands mutate this and persist to disk.
    pub(crate) playlist_store: Arc<Mutex<PlaylistStore>>,
    /// Shared metadata keyed by exact image content hash.
    pub(crate) content_store: Arc<Mutex<ContentStore>>,
    /// Path to the playlists KDL file on disk.
    pub(super) playlists_path: Arc<std::path::PathBuf>,
    /// Path to the versioned content metadata sidecar.
    pub(super) content_path: Arc<std::path::PathBuf>,
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

    pub(super) fn replace_sources(&self, new_index: PhotoIndex, new_roots: Vec<PathBuf>) -> u64 {
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

    pub(super) fn resolve_current_content_target(&self, target: &str) -> anyhow::Result<String> {
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

    pub(super) fn update_content_target(
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

    pub(super) fn resolve_playlist_content(
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

    pub(super) fn update_content<T>(
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

    pub(super) fn persist_playlist_and_content(
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

    pub(super) fn update_playlists<T>(
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

    pub(super) fn recover_pending_transaction(&self) -> anyhow::Result<()> {
        recover_playlist_content_transaction(
            self.playlists_path.as_ref(),
            self.content_path.as_ref(),
        )
    }

    pub(super) fn update_playlist_entry<T>(
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

    pub(super) fn enqueue_swap(&self, request: SwapRequest) -> anyhow::Result<()> {
        self.swap_tx.try_send(request).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => anyhow::anyhow!("runtime swap queue is busy"),
            mpsc::error::TrySendError::Closed(_) => {
                anyhow::anyhow!("runtime swap channel is closed")
            }
        })
    }
}

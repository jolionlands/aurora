use serde::{Deserialize, Serialize};

pub const DEFAULT_PLAYLIST_SHOW_LIMIT: usize = 100;
pub const MAX_PLAYLIST_SHOW_LIMIT: usize = 256;

// ---------------------------------------------------------------------------
// IPC message enum (client → daemon)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum IpcMessage {
    /// Query current daemon state.
    #[serde(rename = "status")]
    Status,

    /// Skip to the next photo immediately.
    #[serde(rename = "next")]
    Next,

    /// Restore the previous photo.
    #[serde(rename = "prev")]
    Prev,

    /// Pause automatic cycling.  `duration_secs = None` means indefinite.
    #[serde(rename = "pause")]
    Pause {
        #[serde(default)]
        duration_secs: Option<u64>,
    },

    /// Resume automatic cycling.
    #[serde(rename = "resume")]
    Resume,

    /// Apply a specific photo by path.
    #[serde(rename = "set")]
    Set { path: String },

    /// Narrow the active source to a specific folder for this session.
    #[serde(rename = "set_folder")]
    SetFolder { path: String },

    /// Ban a photo by its content hash so it is never shown again.
    #[serde(rename = "ban")]
    Ban { hash: String },

    /// Return a metrics/stats dump.
    #[serde(rename = "stats")]
    Stats,

    /// Reload photo sources from disk. Schedule, transition, monitor, cache,
    /// metrics, and log-level settings still require a daemon restart.
    #[serde(rename = "reload")]
    Reload,

    /// Gracefully stop the daemon.
    #[serde(rename = "quit")]
    Quit,

    /// Subscribe to a stream of IPC events.  The connection stays open and
    /// each subsequent JSON object on it is an `IpcEvent`.
    #[serde(rename = "subscribe_events")]
    SubscribeEvents {
        #[serde(default)]
        types: Vec<String>,
    },

    /// Query the last successfully applied wallpaper snapshot for each monitor.
    #[serde(rename = "get_current_wallpaper")]
    GetCurrentWallpaper,

    // ------------------------------------------------------------------
    // Playlist management
    // ------------------------------------------------------------------
    /// List all playlists and the active one.
    #[serde(rename = "playlist_list")]
    PlaylistList,

    /// Return one bounded page of a playlist and its metadata.
    #[serde(rename = "playlist_show")]
    PlaylistShow {
        name: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_playlist_show_limit")]
        limit: usize,
    },

    /// Create an empty playlist.
    #[serde(rename = "playlist_create")]
    PlaylistCreate { name: String },

    /// Add a path to a playlist.
    #[serde(rename = "playlist_add")]
    PlaylistAdd { name: String, path: String },

    /// Replace tags on a playlist path.
    #[serde(rename = "playlist_tag")]
    PlaylistTag {
        name: String,
        path: String,
        #[serde(default = "default_tag_kind")]
        kind: String,
        tags: Vec<String>,
    },

    /// Set a playlist path rating.
    #[serde(rename = "playlist_rate")]
    PlaylistRate {
        name: String,
        path: String,
        rating: u8,
    },

    /// Set a playlist path frequency weight.
    #[serde(rename = "playlist_frequency")]
    PlaylistFrequency {
        name: String,
        path: String,
        frequency: u32,
    },

    /// Enable or disable shuffled selection for a playlist.
    #[serde(rename = "playlist_shuffle")]
    PlaylistShuffle { name: String, shuffle: bool },

    /// Check whether one playlist path already has autotag metadata.
    #[serde(rename = "playlist_autotag_status")]
    PlaylistAutotagStatus { name: String, path: String },

    /// Atomically add one path and apply all of its autotag metadata.
    #[serde(rename = "playlist_autotag_upsert")]
    PlaylistAutotagUpsert {
        name: String,
        path: String,
        groups: std::collections::BTreeMap<String, Vec<String>>,
        #[serde(default)]
        rating: Option<u8>,
        #[serde(default)]
        frequency: Option<u32>,
        #[serde(default)]
        create_playlist: bool,
        #[serde(default)]
        overwrite_existing: bool,
    },

    /// Remove a path from a playlist.
    #[serde(rename = "playlist_remove")]
    PlaylistRemove { name: String, path: String },

    /// Set the active playlist and request an immediate wallpaper swap.
    #[serde(rename = "playlist_activate")]
    PlaylistActivate { name: String },

    /// Clear the active playlist (return to full-index rotation).
    #[serde(rename = "playlist_deactivate")]
    PlaylistDeactivate,

    /// Delete a playlist.
    #[serde(rename = "playlist_delete")]
    PlaylistDelete { name: String },
}

fn default_tag_kind() -> String {
    "general".to_string()
}

fn default_playlist_show_limit() -> usize {
    DEFAULT_PLAYLIST_SHOW_LIMIT
}

// ---------------------------------------------------------------------------
// IPC event enum (daemon → subscribed clients)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum IpcEvent {
    /// Emitted each time a wallpaper swap completes on a monitor.
    #[serde(rename = "swapped")]
    Swapped {
        monitor: String,
        path: String,
        ts_ms: u64,
    },

    /// Automatic cycling has been paused.
    #[serde(rename = "paused")]
    Paused,

    /// Automatic cycling has been resumed.
    #[serde(rename = "resumed")]
    Resumed,

    /// Configuration was reloaded from disk.
    #[serde(rename = "config_reloaded")]
    ConfigReloaded,

    /// Emitted each time a wallpaper is successfully applied to a monitor.
    #[serde(rename = "wallpaper_changed")]
    WallpaperChanged { monitor_id: String, path: String },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_roundtrip() {
        let msg = IpcMessage::Status;
        let raw = serde_json::to_string(&msg).unwrap();
        assert!(
            raw.contains("\"status\""),
            "tag should be 'status': {}",
            raw
        );
        let parsed: IpcMessage = serde_json::from_str(&raw).unwrap();
        assert!(matches!(parsed, IpcMessage::Status));
    }

    #[test]
    fn test_pause_no_duration() {
        let msg = IpcMessage::Pause {
            duration_secs: None,
        };
        let raw = serde_json::to_string(&msg).unwrap();
        let parsed: IpcMessage = serde_json::from_str(&raw).unwrap();
        match parsed {
            IpcMessage::Pause { duration_secs } => assert!(duration_secs.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_pause_with_duration() {
        let msg = IpcMessage::Pause {
            duration_secs: Some(3600),
        };
        let raw = serde_json::to_string(&msg).unwrap();
        let parsed: IpcMessage = serde_json::from_str(&raw).unwrap();
        match parsed {
            IpcMessage::Pause { duration_secs } => assert_eq!(duration_secs, Some(3600)),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_playlist_shuffle_roundtrip() {
        let raw = serde_json::to_string(&IpcMessage::PlaylistShuffle {
            name: "focus".to_string(),
            shuffle: true,
        })
        .unwrap();
        assert!(matches!(
            serde_json::from_str(&raw).unwrap(),
            IpcMessage::PlaylistShuffle {
                name,
                shuffle: true
            } if name == "focus"
        ));
    }

    #[test]
    fn test_playlist_show_roundtrip() {
        let raw = serde_json::to_string(&IpcMessage::PlaylistShow {
            name: "focus".to_string(),
            offset: 12,
            limit: 64,
        })
        .unwrap();
        assert!(matches!(
            serde_json::from_str(&raw).unwrap(),
            IpcMessage::PlaylistShow {
                name,
                offset: 12,
                limit: 64,
            } if name == "focus"
        ));

        let defaulted: IpcMessage =
            serde_json::from_str(r#"{"type":"playlist_show","data":{"name":"focus"}}"#).unwrap();
        assert!(matches!(
            defaulted,
            IpcMessage::PlaylistShow {
                offset: 0,
                limit: DEFAULT_PLAYLIST_SHOW_LIMIT,
                ..
            }
        ));
    }

    #[test]
    fn test_event_swapped_roundtrip() {
        let ev = IpcEvent::Swapped {
            monitor: r"\\.\DISPLAY1\Monitor0".to_string(),
            path: r"C:\Pictures\foo.jpg".to_string(),
            ts_ms: 1_700_000_000_000,
        };
        let raw = serde_json::to_string(&ev).unwrap();
        let parsed: IpcEvent = serde_json::from_str(&raw).unwrap();
        match parsed {
            IpcEvent::Swapped {
                monitor,
                path,
                ts_ms,
            } => {
                assert_eq!(monitor, r"\\.\DISPLAY1\Monitor0");
                assert_eq!(ts_ms, 1_700_000_000_000);
                let _ = path;
            }
            _ => panic!("wrong variant"),
        }
    }
}

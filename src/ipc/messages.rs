use serde::{Deserialize, Serialize};

pub const DEFAULT_PLAYLIST_SHOW_LIMIT: usize = 100;
pub const MAX_PLAYLIST_SHOW_LIMIT: usize = 256;
pub const DEFAULT_CONTENT_LIST_LIMIT: usize = DEFAULT_PLAYLIST_SHOW_LIMIT;
pub const MAX_CONTENT_LIST_LIMIT: usize = MAX_PLAYLIST_SHOW_LIMIT;

// ---------------------------------------------------------------------------
// IPC message enum (client → daemon)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum IpcMessage {
    /// Query current daemon state.
    Status,

    /// Skip to the next photo immediately.
    Next,

    /// Restore the previous photo.
    Prev,

    /// Pause automatic cycling.  `duration_secs = None` means indefinite.
    Pause {
        #[serde(default)]
        duration_secs: Option<u64>,
    },

    /// Resume automatic cycling.
    Resume,

    /// Apply a specific photo by path.
    Set { path: String },

    /// Narrow the active source to a specific folder for this session.
    SetFolder { path: String },

    /// Ban a photo by its content hash so it is never shown again.
    Ban { hash: String },

    /// Return a metrics/stats dump.
    Stats,

    /// Reload photo sources from disk. Schedule, transition, monitor, cache,
    /// metrics, and log-level settings still require a daemon restart.
    Reload,

    /// Gracefully stop the daemon.
    Quit,

    /// Subscribe to a stream of IPC events.  The connection stays open and
    /// each subsequent JSON object on it is an `IpcEvent`.
    SubscribeEvents {
        #[serde(default)]
        types: Vec<String>,
    },

    /// Query the last successfully applied wallpaper snapshot for each monitor.
    GetCurrentWallpaper,

    // ------------------------------------------------------------------
    // Playlist management
    // ------------------------------------------------------------------
    /// List all playlists and the active one.
    PlaylistList,

    /// Return one bounded page of a playlist and its metadata.
    PlaylistShow {
        name: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_playlist_show_limit")]
        limit: usize,
    },

    /// Create an empty static or dynamic playlist.
    PlaylistCreate {
        name: String,
        #[serde(default)]
        dynamic: bool,
    },

    /// Add a path to a playlist.
    PlaylistAdd { name: String, path: String },

    /// Replace tags on a playlist path.
    PlaylistTag {
        name: String,
        path: String,
        #[serde(default = "default_tag_kind")]
        kind: String,
        tags: Vec<String>,
    },

    /// Set a playlist path rating.
    PlaylistRate {
        name: String,
        path: String,
        rating: u8,
    },

    /// Set a playlist path frequency weight.
    PlaylistFrequency {
        name: String,
        path: String,
        frequency: u32,
    },

    /// Enable or disable shuffled selection for a playlist.
    PlaylistShuffle { name: String, shuffle: bool },

    /// Replace the active include/exclude tag filters for a playlist.
    PlaylistFilter {
        name: String,
        #[serde(default)]
        include: std::collections::BTreeMap<String, Vec<String>>,
        #[serde(default)]
        exclude: std::collections::BTreeMap<String, Vec<String>>,
    },

    /// Check whether one playlist path already has autotag metadata.
    PlaylistAutotagStatus { name: String, path: String },

    /// Atomically add one path and apply all of its autotag metadata.
    PlaylistAutotagUpsert {
        name: String,
        path: String,
        groups: std::collections::BTreeMap<String, Vec<String>>,
        #[serde(default)]
        rating: Option<u8>,
        #[serde(default)]
        frequency: Option<u32>,
        #[serde(default)]
        provenance: Option<crate::content::AutoTagProvenance>,
        #[serde(default)]
        create_playlist: bool,
        #[serde(default)]
        overwrite_existing: bool,
    },

    /// Remove a path from a playlist.
    PlaylistRemove { name: String, path: String },

    /// Set the active playlist and request an immediate wallpaper swap.
    PlaylistActivate { name: String },

    /// Clear the active playlist (return to full-index rotation).
    PlaylistDeactivate,

    /// Delete a playlist.
    PlaylistDelete { name: String },

    // ------------------------------------------------------------------
    // Content metadata
    // ------------------------------------------------------------------
    /// Return one bounded page of content metadata.
    ContentList {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_content_list_limit")]
        limit: usize,
        #[serde(default)]
        include: std::collections::BTreeMap<String, Vec<String>>,
        #[serde(default)]
        exclude: std::collections::BTreeMap<String, Vec<String>>,
    },

    /// Return metadata for one content ID, alias, or path.
    ContentShow { target: String },

    /// Replace one tag group for a content item.
    ContentTag {
        target: String,
        #[serde(default = "default_tag_kind")]
        kind: String,
        tags: Vec<String>,
    },

    /// Set a content item's rating.
    ContentRate { target: String, rating: u8 },

    /// Clear all shared metadata for a content item.
    ContentClear { target: String },
}

fn default_tag_kind() -> String {
    "general".to_string()
}

fn default_playlist_show_limit() -> usize {
    DEFAULT_PLAYLIST_SHOW_LIMIT
}

fn default_content_list_limit() -> usize {
    DEFAULT_CONTENT_LIST_LIMIT
}

// ---------------------------------------------------------------------------
// IPC event enum (daemon → subscribed clients)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum IpcEvent {
    /// Emitted each time a wallpaper swap completes on a monitor.
    Swapped {
        monitor: String,
        path: String,
        ts_ms: u64,
    },

    /// Automatic cycling has been paused.
    Paused,

    /// Automatic cycling has been resumed.
    Resumed,

    /// Configuration was reloaded from disk.
    ConfigReloaded,

    /// Emitted each time a wallpaper is successfully applied to a monitor.
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
    fn playlist_create_defaults_to_static() {
        let defaulted: IpcMessage =
            serde_json::from_str(r#"{"type":"playlist_create","data":{"name":"focus"}}"#).unwrap();
        assert!(matches!(
            defaulted,
            IpcMessage::PlaylistCreate {
                name,
                dynamic: false
            } if name == "focus"
        ));

        let raw = serde_json::to_string(&IpcMessage::PlaylistCreate {
            name: "fresh".to_string(),
            dynamic: true,
        })
        .unwrap();
        assert!(matches!(
            serde_json::from_str(&raw).unwrap(),
            IpcMessage::PlaylistCreate {
                name,
                dynamic: true
            } if name == "fresh"
        ));
    }

    #[test]
    fn playlist_filter_roundtrip() {
        let message = IpcMessage::PlaylistFilter {
            name: "focus".to_string(),
            include: std::collections::BTreeMap::from([(
                "theme".to_string(),
                vec!["night".to_string()],
            )]),
            exclude: std::collections::BTreeMap::from([(
                "safety".to_string(),
                vec!["nsfw".to_string()],
            )]),
        };

        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: IpcMessage = serde_json::from_str(&encoded).unwrap();

        assert!(matches!(
            decoded,
            IpcMessage::PlaylistFilter {
                name,
                include,
                exclude
            } if name == "focus"
                && include["theme"] == ["night"]
                && exclude["safety"] == ["nsfw"]
        ));
    }

    #[test]
    fn content_list_roundtrip_and_defaults() {
        let message = IpcMessage::ContentList {
            offset: 12,
            limit: 64,
            include: std::collections::BTreeMap::from([(
                "theme".to_string(),
                vec!["night".to_string()],
            )]),
            exclude: std::collections::BTreeMap::new(),
        };
        let raw = serde_json::to_string(&message).unwrap();
        assert!(matches!(
            serde_json::from_str(&raw).unwrap(),
            IpcMessage::ContentList {
                offset: 12,
                limit: 64,
                include,
                exclude,
            } if include["theme"] == ["night"] && exclude.is_empty()
        ));

        let defaulted: IpcMessage =
            serde_json::from_str(r#"{"type":"content_list","data":{}}"#).unwrap();
        assert!(matches!(
            defaulted,
            IpcMessage::ContentList {
                offset: 0,
                limit: DEFAULT_CONTENT_LIST_LIMIT,
                include,
                exclude,
            } if include.is_empty() && exclude.is_empty()
        ));
    }

    #[test]
    fn content_mutation_roundtrip() {
        let messages = [
            IpcMessage::ContentShow {
                target: "blake3:abc".to_string(),
            },
            IpcMessage::ContentTag {
                target: "blake3:abc".to_string(),
                kind: "theme".to_string(),
                tags: vec!["night".to_string()],
            },
            IpcMessage::ContentRate {
                target: "blake3:abc".to_string(),
                rating: 4,
            },
            IpcMessage::ContentClear {
                target: "blake3:abc".to_string(),
            },
        ];

        for message in messages {
            let raw = serde_json::to_string(&message).unwrap();
            let decoded: IpcMessage = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                serde_json::to_value(decoded).unwrap(),
                serde_json::to_value(message).unwrap()
            );
        }
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

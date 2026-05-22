use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum IpcError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Read error: {0}")]
    ReadError(String),
    #[error("Write error: {0}")]
    WriteError(String),
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("Pipe creation failed: {0}")]
    PipeCreationFailed(String),
    #[error("Client disconnected")]
    ClientDisconnected,
    #[error("Timeout")]
    Timeout,
    #[error("Invalid message: {0}")]
    InvalidMessage(String),
    #[error("Windows API error: {0}")]
    WindowsError(String),
}

impl From<std::io::Error> for IpcError {
    fn from(e: std::io::Error) -> Self {
        IpcError::ReadError(e.to_string())
    }
}

impl From<serde_json::Error> for IpcError {
    fn from(e: serde_json::Error) -> Self {
        IpcError::SerializationError(e.to_string())
    }
}

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

    /// Rate the currently displayed photo (1–5 stars).
    #[serde(rename = "rate")]
    Rate { stars: u8 },

    /// Ban a photo by its content hash so it is never shown again.
    #[serde(rename = "ban")]
    Ban { hash: String },

    /// Return a metrics/stats dump.
    #[serde(rename = "stats")]
    Stats,

    /// Reload configuration from disk.
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

    /// Query the currently displayed wallpaper path for each monitor.
    #[serde(rename = "get_current_wallpaper")]
    GetCurrentWallpaper,
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
    WallpaperChanged {
        monitor_id: String,
        path: String,
    },
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
        assert!(raw.contains("\"status\""), "tag should be 'status': {}", raw);
        let parsed: IpcMessage = serde_json::from_str(&raw).unwrap();
        assert!(matches!(parsed, IpcMessage::Status));
    }

    #[test]
    fn test_pause_no_duration() {
        let msg = IpcMessage::Pause { duration_secs: None };
        let raw = serde_json::to_string(&msg).unwrap();
        let parsed: IpcMessage = serde_json::from_str(&raw).unwrap();
        match parsed {
            IpcMessage::Pause { duration_secs } => assert!(duration_secs.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_pause_with_duration() {
        let msg = IpcMessage::Pause { duration_secs: Some(3600) };
        let raw = serde_json::to_string(&msg).unwrap();
        let parsed: IpcMessage = serde_json::from_str(&raw).unwrap();
        match parsed {
            IpcMessage::Pause { duration_secs } => assert_eq!(duration_secs, Some(3600)),
            _ => panic!("wrong variant"),
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
            IpcEvent::Swapped { monitor, path, ts_ms } => {
                assert_eq!(monitor, r"\\.\DISPLAY1\Monitor0");
                assert_eq!(ts_ms, 1_700_000_000_000);
                let _ = path;
            }
            _ => panic!("wrong variant"),
        }
    }
}

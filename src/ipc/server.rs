use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, info, warn};

use super::messages::{IpcEvent, IpcMessage};
use super::{
    pipe_path, read_frame_with_timeout, write_frame_with_timeout, FRAME_IO_TIMEOUT, MAX_FRAME_SIZE,
};
use crate::runtime::RuntimeHandle;

const MAX_CONCURRENT_HANDLERS: usize = 32;
const MAX_EVENT_SUBSCRIBERS: usize = 16;

const EVENT_SWAPPED: &str = "swapped";
const EVENT_PAUSED: &str = "paused";
const EVENT_RESUMED: &str = "resumed";
const EVENT_CONFIG_RELOADED: &str = "config_reloaded";
const EVENT_WALLPAPER_CHANGED: &str = "wallpaper_changed";
const VALID_EVENT_TYPES: &[&str] = &[
    EVENT_SWAPPED,
    EVENT_PAUSED,
    EVENT_RESUMED,
    EVENT_CONFIG_RELOADED,
    EVENT_WALLPAPER_CHANGED,
];

// ---------------------------------------------------------------------------
// IpcServer
// ---------------------------------------------------------------------------

pub struct IpcServer {
    pipe_path: String,
    initial_pipe: parking_lot::Mutex<Option<tokio::net::windows::named_pipe::NamedPipeServer>>,
    event_tx: broadcast::Sender<IpcEvent>,
    shutdown_tx: parking_lot::Mutex<Option<oneshot::Sender<()>>>,
    runtime: parking_lot::Mutex<Option<RuntimeHandle>>,
    handler_slots: Arc<Semaphore>,
    subscriber_slots: Arc<Semaphore>,
}

impl IpcServer {
    /// Reserve the session's authoritative endpoint before daemon startup work.
    pub fn bind() -> Result<Self> {
        let pipe_path = pipe_path()?;
        let initial_pipe = create_pipe_with_sa(&pipe_path, true).map_err(|e| {
            anyhow::anyhow!(
                "cannot reserve IPC endpoint {pipe_path}; another instance may already be running: {e}"
            )
        })?;
        let (event_tx, _) = broadcast::channel(64);
        Ok(Self {
            pipe_path,
            initial_pipe: parking_lot::Mutex::new(Some(initial_pipe)),
            event_tx,
            shutdown_tx: parking_lot::Mutex::new(None),
            runtime: parking_lot::Mutex::new(None),
            handler_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS)),
            subscriber_slots: Arc::new(Semaphore::new(MAX_EVENT_SUBSCRIBERS)),
        })
    }

    /// Wire a RuntimeHandle so IPC commands can drive the runtime.
    pub fn set_runtime(&self, handle: RuntimeHandle) {
        *self.runtime.lock() = Some(handle);
    }

    pub fn broadcast_event(&self, event: IpcEvent) {
        let _ = self.event_tx.send(event);
    }

    pub fn set_shutdown_sender(&self, tx: oneshot::Sender<()>) {
        *self.shutdown_tx.lock() = Some(tx);
    }

    /// Clone the broadcast sender so Runtime can emit events into it.
    pub fn event_tx_clone(&self) -> broadcast::Sender<IpcEvent> {
        self.event_tx.clone()
    }

    /// Start the accept loop.  Returns only on fatal pipe error.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        info!(
            "IPC server starting on {} (ACL: Admins+Owner only)",
            self.pipe_path
        );

        const MAX_RETRIES: u32 = 5;
        let mut retry_count = 0u32;
        let mut initial_pipe = Some(
            self.initial_pipe
                .lock()
                .take()
                .ok_or_else(|| anyhow::anyhow!("IPC server already started"))?,
        );

        loop {
            let server = if let Some(server) = initial_pipe.take() {
                server
            } else {
                match create_pipe_with_sa(&self.pipe_path, false) {
                    Ok(s) => {
                        retry_count = 0;
                        s
                    }
                    Err(e) => {
                        retry_count += 1;
                        warn!(
                            "Pipe create failed ({}/{}): {}",
                            retry_count, MAX_RETRIES, e
                        );
                        if retry_count >= MAX_RETRIES {
                            error!(
                                "IPC pipe creation failed {} times — giving up.",
                                MAX_RETRIES
                            );
                            return Err(anyhow::anyhow!("Cannot create IPC pipe: {}", e));
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
            };

            if let Err(e) = server.connect().await {
                warn!("Pipe connect error: {}", e);
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                continue;
            }

            info!("IPC client connected");
            let permit = Arc::clone(&self.handler_slots)
                .acquire_owned()
                .await
                .map_err(|_| anyhow::anyhow!("IPC handler semaphore closed"))?;
            let ipc = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(e) = ipc.handle_client(server, permit).await {
                    debug!("IPC client handler error: {}", e);
                }
            });
        }
    }

    async fn handle_client(
        &self,
        mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
        _permit: OwnedSemaphorePermit,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let payload = match read_frame_with_timeout(&mut pipe, FRAME_IO_TIMEOUT).await? {
            Some(payload) => payload,
            None => {
                info!("IPC client disconnected");
                return Ok(());
            }
        };

        let message: IpcMessage = match serde_json::from_slice(&payload) {
            Ok(m) => m,
            Err(e) => {
                warn!("IPC: Failed to parse message: {}", e);
                let err = serde_json::json!({
                    "success": false,
                    "error": format!("Invalid message: {}", e)
                });
                let bytes = serde_json::to_vec(&err)?;
                let _ = write_frame_with_timeout(&mut pipe, &bytes, FRAME_IO_TIMEOUT).await;
                let _ = pipe.shutdown().await;
                return Ok(());
            }
        };

        debug!("IPC received: {:?}", message);
        let is_playlist_list = matches!(&message, IpcMessage::PlaylistList);

        // SubscribeEvents: reserve and subscribe before ACK, then stream until disconnect.
        if let IpcMessage::SubscribeEvents { ref types } = message {
            info!("IPC: SubscribeEvents {:?}", types);
            let subscriber_permit = match validate_event_filter(types)
                .and_then(|()| acquire_subscriber_slot(&self.subscriber_slots))
            {
                Ok(permit) => permit,
                Err(error) => {
                    let response = serde_json::to_vec(&serde_json::json!({
                        "success": false,
                        "error": error.to_string(),
                    }))?;
                    write_frame_with_timeout(&mut pipe, &response, FRAME_IO_TIMEOUT).await?;
                    return Ok(());
                }
            };
            let type_filter = types.clone();
            let rx = acknowledge_subscription(
                &mut pipe,
                _permit,
                self.event_tx.clone(),
                FRAME_IO_TIMEOUT,
            )
            .await?;
            return stream_subscription(pipe, rx, type_filter, subscriber_permit).await;
        }

        let (response, shutdown_tx) = match message {
            IpcMessage::Quit => {
                info!("IPC: Quit requested");
                match self.shutdown_tx.lock().take() {
                    Some(tx) => (serde_json::json!({ "success": true }), Some(tx)),
                    None => (
                        serde_json::json!({
                            "success": false,
                            "error": "shutdown already requested or unavailable"
                        }),
                        None,
                    ),
                }
            }
            message => (self.process_message(message).await, None),
        };
        let bytes = bounded_response_bytes(&response, is_playlist_list)?;
        write_frame_with_timeout(&mut pipe, &bytes, FRAME_IO_TIMEOUT).await?;
        let _ = pipe.shutdown().await;
        if let Some(tx) = shutdown_tx {
            let _ = tx.send(());
        }
        Ok(())
    }

    async fn process_message(&self, message: IpcMessage) -> serde_json::Value {
        // Helper: get a clone of the runtime handle, or return not-ready error.
        macro_rules! runtime {
            () => {
                match self.runtime.lock().clone() {
                    Some(h) => h,
                    None => {
                        return serde_json::json!({
                            "success": false,
                            "error": "runtime not yet initialised"
                        })
                    }
                }
            };
        }

        match message {
            IpcMessage::Status => {
                let result = match self.runtime.lock().clone() {
                    Some(h) => h.status(),
                    None => serde_json::json!({ "running": true }),
                };
                serde_json::json!({ "success": true, "result": result })
            }

            IpcMessage::Stats => {
                let result = match self.runtime.lock().clone() {
                    Some(h) => h.stats(),
                    None => serde_json::json!({}),
                };
                serde_json::json!({ "success": true, "result": result })
            }

            IpcMessage::Reload => {
                info!("IPC: Reload config requested");
                let handle = { self.runtime.lock().clone() };
                if let Some(handle) = handle {
                    match tokio::task::spawn_blocking(move || handle.reload_from_disk()).await {
                        Ok(Ok(())) => {
                            self.broadcast_event(IpcEvent::ConfigReloaded);
                            serde_json::json!({
                                "success": true,
                                "result": {
                                    "index_reloaded": true,
                                    "restart_required": ["schedule", "transitions", "monitors", "cache", "metrics", "log-level"]
                                }
                            })
                        }
                        Ok(Err(error)) => {
                            serde_json::json!({ "success": false, "error": error.to_string() })
                        }
                        Err(error) => serde_json::json!({
                            "success": false,
                            "error": format!("reload task failed: {error}")
                        }),
                    }
                } else {
                    serde_json::json!({ "success": false, "error": "runtime not initialised" })
                }
            }

            IpcMessage::Quit => {
                serde_json::json!({ "success": false, "error": "quit requires an IPC connection" })
            }

            IpcMessage::Next => command_response(runtime!().skip_next()),

            IpcMessage::Prev => command_response(runtime!().prev()),

            IpcMessage::Pause { duration_secs } => {
                let h = runtime!();
                let dur = duration_secs.map(std::time::Duration::from_secs);
                h.pause(dur);
                self.broadcast_event(IpcEvent::Paused);
                serde_json::json!({ "success": true })
            }

            IpcMessage::Resume => {
                let h = runtime!();
                h.resume();
                self.broadcast_event(IpcEvent::Resumed);
                serde_json::json!({ "success": true })
            }

            IpcMessage::Set { path } => {
                command_response(runtime!().set_specific(std::path::PathBuf::from(path)))
            }

            IpcMessage::SetFolder { path } => {
                let handle = { self.runtime.lock().clone() };
                if let Some(handle) = handle {
                    match tokio::task::spawn_blocking(move || {
                        handle.set_folder(std::path::PathBuf::from(path))
                    })
                    .await
                    {
                        Ok(result) => command_response(result),
                        Err(error) => serde_json::json!({
                            "success": false,
                            "error": format!("set-folder task failed: {error}")
                        }),
                    }
                } else {
                    serde_json::json!({ "success": false, "error": "runtime not initialised" })
                }
            }

            IpcMessage::Ban { hash } => command_response(runtime!().ban(&hash)),

            IpcMessage::SubscribeEvents { .. } => {
                // Handled above in handle_client before reaching process_message.
                serde_json::json!({ "success": true, "subscription_id": 1 })
            }

            IpcMessage::GetCurrentWallpaper => {
                if let Some(handle) = self.runtime.lock().clone() {
                    let map = handle.current_wallpaper();
                    let entries: serde_json::Map<String, serde_json::Value> = map
                        .into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v.display().to_string())))
                        .collect();
                    serde_json::json!({ "success": true, "result": entries })
                } else {
                    serde_json::json!({ "success": false, "error": "runtime not initialised" })
                }
            }

            // ------------------------------------------------------------------
            // Playlist management
            // ------------------------------------------------------------------
            IpcMessage::PlaylistList => {
                let h = runtime!();
                let result = h.playlist_list();
                serde_json::json!({ "success": true, "result": result })
            }

            IpcMessage::PlaylistShow {
                name,
                offset,
                limit,
            } => {
                let h = runtime!();
                match h.playlist_show(&name, offset, limit) {
                    Ok(result) => serde_json::json!({ "success": true, "result": result }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistCreate { name } => {
                command_response(runtime!().playlist_create(&name))
            }

            IpcMessage::PlaylistAdd { name, path } => {
                command_response(runtime!().playlist_add(&name, &path))
            }

            IpcMessage::PlaylistTag {
                name,
                path,
                kind,
                tags,
            } => command_response(runtime!().playlist_tag(&name, &path, &kind, tags)),

            IpcMessage::PlaylistRate { name, path, rating } => {
                command_response(runtime!().playlist_rate(&name, &path, rating))
            }

            IpcMessage::PlaylistFrequency {
                name,
                path,
                frequency,
            } => command_response(runtime!().playlist_frequency(&name, &path, frequency)),

            IpcMessage::PlaylistShuffle { name, shuffle } => {
                command_response(runtime!().playlist_shuffle(&name, shuffle))
            }

            IpcMessage::PlaylistAutotagStatus { name, path } => {
                let h = runtime!();
                match h.playlist_autotag_status(&name, &path) {
                    Ok(has_metadata) => serde_json::json!({
                        "success": true,
                        "result": { "has_metadata": has_metadata },
                    }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistAutotagUpsert {
                name,
                path,
                groups,
                rating,
                frequency,
                create_playlist,
                overwrite_existing,
            } => {
                let h = runtime!();
                match h.playlist_autotag_upsert(
                    &name,
                    &path,
                    groups,
                    rating,
                    frequency,
                    create_playlist,
                    overwrite_existing,
                ) {
                    Ok(applied) => serde_json::json!({
                        "success": true,
                        "result": { "applied": applied },
                    }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistRemove { name, path } => {
                command_response(runtime!().playlist_remove(&name, &path))
            }

            IpcMessage::PlaylistActivate { name } => {
                command_response(runtime!().playlist_activate(&name))
            }

            IpcMessage::PlaylistDeactivate => command_response(runtime!().playlist_deactivate()),

            IpcMessage::PlaylistDelete { name } => {
                command_response(runtime!().playlist_delete(&name))
            }
        }
    }
}

fn command_response(result: Result<()>) -> serde_json::Value {
    match result {
        Ok(()) => serde_json::json!({ "success": true }),
        Err(error) => serde_json::json!({ "success": false, "error": error.to_string() }),
    }
}

fn bounded_response_bytes(response: &serde_json::Value, is_playlist_list: bool) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(response)?;
    if bytes.len() <= MAX_FRAME_SIZE {
        return Ok(bytes);
    }
    let error = if is_playlist_list {
        format!(
            "playlist_list response exceeds the {MAX_FRAME_SIZE}-byte IPC limit; use targeted playlist commands"
        )
    } else {
        format!("IPC response exceeds the {MAX_FRAME_SIZE}-byte limit")
    };
    Ok(serde_json::to_vec(&serde_json::json!({
        "success": false,
        "error": error,
    }))?)
}

async fn acknowledge_subscription<W>(
    writer: &mut W,
    permit: OwnedSemaphorePermit,
    event_tx: broadcast::Sender<IpcEvent>,
    timeout: std::time::Duration,
) -> Result<broadcast::Receiver<IpcEvent>>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let rx = event_tx.subscribe();
    let ack = serde_json::to_vec(&serde_json::json!({"success": true, "subscription_id": 1}))?;
    write_frame_with_timeout(writer, &ack, timeout).await?;
    drop(permit);
    Ok(rx)
}

async fn stream_subscription<P>(
    mut pipe: P,
    mut rx: broadcast::Receiver<IpcEvent>,
    type_filter: Vec<String>,
    _subscriber_permit: OwnedSemaphorePermit,
) -> Result<()>
where
    P: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncReadExt;

    loop {
        let mut probe = [0u8; 1];
        let event = tokio::select! {
            biased;
            read = pipe.read(&mut probe) => {
                match read {
                    Ok(0) => debug!("IPC event subscriber disconnected"),
                    Ok(_) => debug!("IPC event subscriber sent unexpected data"),
                    Err(error) => debug!("IPC event subscriber read ended: {}", error),
                }
                return Ok(());
            }
            event = rx.recv() => event,
        };

        match event {
            Ok(event) => {
                if !type_filter.is_empty()
                    && !type_filter
                        .iter()
                        .any(|wanted| wanted == event_type(&event))
                {
                    continue;
                }
                let bytes = match serde_json::to_vec(&event) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        warn!("IPC: serialize event: {}", error);
                        continue;
                    }
                };
                if let Err(error) =
                    write_frame_with_timeout(&mut pipe, &bytes, FRAME_IO_TIMEOUT).await
                {
                    debug!("IPC event subscriber disconnected: {}", error);
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                warn!("IPC event subscriber lagged by {} events", count);
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("IPC event broadcast channel closed");
                return Ok(());
            }
        }
    }
}

fn validate_event_filter(types: &[String]) -> Result<()> {
    let invalid: Vec<_> = types
        .iter()
        .map(String::as_str)
        .filter(|event_type| !VALID_EVENT_TYPES.contains(event_type))
        .collect();
    if !invalid.is_empty() {
        anyhow::bail!(
            "invalid event type(s): {}; valid event types: {}",
            invalid.join(", "),
            VALID_EVENT_TYPES.join(", ")
        );
    }
    Ok(())
}

fn acquire_subscriber_slot(slots: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit> {
    Arc::clone(slots).try_acquire_owned().map_err(|_| {
        anyhow::anyhow!("event subscriber limit reached (maximum {MAX_EVENT_SUBSCRIBERS})")
    })
}

fn event_type(event: &IpcEvent) -> &'static str {
    match event {
        IpcEvent::Swapped { .. } => EVENT_SWAPPED,
        IpcEvent::Paused => EVENT_PAUSED,
        IpcEvent::Resumed => EVENT_RESUMED,
        IpcEvent::ConfigReloaded => EVENT_CONFIG_RELOADED,
        IpcEvent::WallpaperChanged { .. } => EVENT_WALLPAPER_CHANGED,
    }
}

// RuntimeHandle is also imported at the top of this file. Callers outside the
// module can reach it via `aurora::runtime::RuntimeHandle` since the runtime
// module is `pub` from lib.rs.

// ---------------------------------------------------------------------------
// Pipe creation with ACL (Admins + Owner only)
// ---------------------------------------------------------------------------

fn create_pipe_with_sa(
    path: &str,
    first_instance: bool,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;

    // Never fall back to the default named-pipe ACL: that can grant control to
    // users outside the daemon owner/Administrators SDDL below.
    let sa = build_pipe_security_attributes()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    let sa_ptr = &sa.raw as *const _ as *mut std::ffi::c_void;

    // SAFETY: sa_ptr points to local stack that outlives this
    // synchronous call. The owned descriptor is freed when this function exits.
    Ok(unsafe {
        ServerOptions::new()
            .first_pipe_instance(first_instance)
            .create_with_security_attributes_raw(path, sa_ptr)?
    })
}

struct OwnedPipeSecurityAttributes {
    raw: windows::Win32::Security::SECURITY_ATTRIBUTES,
}

impl Drop for OwnedPipeSecurityAttributes {
    fn drop(&mut self) {
        if !self.raw.lpSecurityDescriptor.is_null() {
            use windows::Win32::Foundation::{LocalFree, HLOCAL};
            // SAFETY: ConvertStringSecurityDescriptorToSecurityDescriptorW
            // allocates this descriptor with LocalAlloc.
            unsafe {
                let _ = LocalFree(HLOCAL(self.raw.lpSecurityDescriptor));
            }
        }
    }
}

fn build_pipe_security_attributes() -> Result<OwnedPipeSecurityAttributes, anyhow::Error> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;

    // D:(A;;GA;;;BA) = Allow GenericAll to BUILTIN\Administrators
    // (A;;GA;;;OW)   = Allow GenericAll to the object Owner
    let sddl: Vec<u16> = "D:(A;;GA;;;BA)(A;;GA;;;OW)\0".encode_utf16().collect();

    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR::from_raw(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
    }

    Ok(OwnedPipeSecurityAttributes {
        raw: windows::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        },
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{read_frame, write_frame};

    fn server_with_swap_sender(
        tx: tokio::sync::mpsc::Sender<crate::scheduler::SwapRequest>,
    ) -> IpcServer {
        let runtime = RuntimeHandle::new(
            tx,
            Arc::new(parking_lot::Mutex::new(
                crate::runtime::RuntimeStateSnapshot::default(),
            )),
            (
                Arc::new(parking_lot::RwLock::new(crate::index::PhotoIndex::default())),
                Arc::new(parking_lot::RwLock::new(Vec::new())),
                crate::runtime::BanGate::default(),
            ),
            crate::metrics::Metrics::new(),
            "config.kdl".into(),
            Arc::new(parking_lot::Mutex::new(
                crate::playlist::PlaylistStore::default(),
            )),
            "playlists.kdl".into(),
        );
        let (event_tx, _) = broadcast::channel(1);
        let server = IpcServer {
            pipe_path: String::new(),
            initial_pipe: parking_lot::Mutex::new(None),
            event_tx,
            shutdown_tx: parking_lot::Mutex::new(None),
            runtime: parking_lot::Mutex::new(None),
            handler_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS)),
            subscriber_slots: Arc::new(Semaphore::new(MAX_EVENT_SUBSCRIBERS)),
        };
        server.set_runtime(runtime);
        server
    }

    #[test]
    fn command_response_preserves_wire_shape() {
        assert_eq!(
            command_response(Ok(())),
            serde_json::json!({ "success": true })
        );
        assert_eq!(
            command_response(Err(anyhow::anyhow!("failed"))),
            serde_json::json!({ "success": false, "error": "failed" })
        );
    }

    #[test]
    fn event_type_matches_wire_names() {
        let events = [
            IpcEvent::Swapped {
                monitor: "m".into(),
                path: "p".into(),
                ts_ms: 1,
            },
            IpcEvent::Paused,
            IpcEvent::Resumed,
            IpcEvent::ConfigReloaded,
            IpcEvent::WallpaperChanged {
                monitor_id: "m".into(),
                path: "p".into(),
            },
        ];
        let wire_types = events
            .iter()
            .map(|event| {
                serde_json::to_value(event).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            wire_types,
            VALID_EVENT_TYPES
                .iter()
                .map(|event_type| (*event_type).to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(events.map(|event| event_type(&event)), VALID_EVENT_TYPES);
    }

    #[test]
    fn event_filters_accept_only_wire_names() {
        let valid = VALID_EVENT_TYPES
            .iter()
            .map(|event_type| (*event_type).to_string())
            .collect::<Vec<_>>();
        assert!(validate_event_filter(&[]).is_ok());
        assert!(validate_event_filter(&valid).is_ok());

        let error = validate_event_filter(&["paused".into(), "not-an-event".into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid event type(s): not-an-event"));
        assert!(error.contains("valid event types: swapped, paused, resumed"));
    }

    #[test]
    fn event_subscriber_count_is_bounded() {
        let slots = Arc::new(Semaphore::new(MAX_EVENT_SUBSCRIBERS));
        let all_slots = Arc::clone(&slots)
            .try_acquire_many_owned(MAX_EVENT_SUBSCRIBERS as u32)
            .unwrap();
        let error = acquire_subscriber_slot(&slots).unwrap_err().to_string();
        assert!(error.contains(&format!("maximum {MAX_EVENT_SUBSCRIBERS}")));
        drop(all_slots);
        assert!(acquire_subscriber_slot(&slots).is_ok());
    }

    #[test]
    fn oversized_playlist_list_becomes_a_bounded_error_response() {
        let response = serde_json::json!({
            "success": true,
            "result": "x".repeat(MAX_FRAME_SIZE),
        });
        let bytes = bounded_response_bytes(&response, true).unwrap();
        assert!(bytes.len() <= MAX_FRAME_SIZE);
        let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["success"], false);
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("playlist_list response exceeds"));
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("targeted playlist commands"));
    }

    #[test]
    fn pipe_security_attributes_build_and_drop() {
        let attrs = build_pipe_security_attributes().expect("build pipe ACL");
        assert!(!attrs.raw.lpSecurityDescriptor.is_null());
    }

    #[tokio::test]
    async fn first_instance_cannot_be_recreated() {
        let path = format!(r"\\.\pipe\aurora-test-first-{}", std::process::id());
        let _owner = create_pipe_with_sa(&path, true).expect("reserve first pipe instance");
        assert!(create_pipe_with_sa(&path, true).is_err());
    }

    #[tokio::test]
    async fn quit_ack_survives_supervisor_shutdown() {
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = format!(r"\\.\pipe\aurora-test-quit-{suffix}");
        let pipe = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&path)
            .expect("create quit test pipe");
        let (event_tx, _) = broadcast::channel(1);
        let server = Arc::new(IpcServer {
            pipe_path: path.clone(),
            initial_pipe: parking_lot::Mutex::new(None),
            event_tx,
            shutdown_tx: parking_lot::Mutex::new(None),
            runtime: parking_lot::Mutex::new(None),
            handler_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS)),
            subscriber_slots: Arc::new(Semaphore::new(MAX_EVENT_SUBSCRIBERS)),
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        server.set_shutdown_sender(shutdown_tx);

        let handler = tokio::spawn({
            let server = Arc::clone(&server);
            async move {
                pipe.connect().await.expect("connect quit test pipe");
                let permit = Arc::clone(&server.handler_slots)
                    .acquire_owned()
                    .await
                    .unwrap();
                server.handle_client(pipe, permit).await
            }
        });
        let handler_abort = handler.abort_handle();
        let supervisor = tokio::spawn(async move {
            shutdown_rx.await.expect("quit signal");
            handler_abort.abort();
        });

        tokio::task::yield_now().await;
        let mut client = ClientOptions::new()
            .open(&path)
            .expect("open quit test pipe");
        write_frame(&mut client, &serde_json::to_vec(&IpcMessage::Quit).unwrap())
            .await
            .expect("write quit request");

        let payload =
            tokio::time::timeout(std::time::Duration::from_secs(1), read_frame(&mut client))
                .await
                .expect("quit response timeout")
                .expect("read quit response")
                .expect("quit response frame");
        let response: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(response["success"], serde_json::json!(true));

        supervisor.await.expect("supervisor task");
        let _ = handler.await;
    }

    #[tokio::test]
    async fn subscription_receiver_precedes_ack_and_releases_handler_permit() {
        let slots = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&slots).acquire_owned().await.unwrap();
        let (mut server_pipe, mut client_pipe) = tokio::io::duplex(1);
        let (event_tx, _) = broadcast::channel(1);
        let ack_events = event_tx.clone();

        let ack_task = tokio::spawn(async move {
            acknowledge_subscription(
                &mut server_pipe,
                permit,
                ack_events,
                std::time::Duration::from_secs(1),
            )
            .await
        });
        tokio::task::yield_now().await;
        assert_eq!(slots.available_permits(), 0);
        assert_eq!(event_tx.receiver_count(), 1);
        event_tx.send(IpcEvent::Paused).unwrap();

        let payload = read_frame_with_timeout(&mut client_pipe, std::time::Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(ack["success"], serde_json::json!(true));
        let mut rx = ack_task.await.unwrap().unwrap();
        assert!(matches!(rx.try_recv(), Ok(IpcEvent::Paused)));
        assert_eq!(slots.available_permits(), 1);

        let permit = Arc::clone(&slots).acquire_owned().await.unwrap();
        let (mut stalled_pipe, _client_pipe) = tokio::io::duplex(1);
        let (stalled_events, _) = broadcast::channel(1);
        let error = acknowledge_subscription(
            &mut stalled_pipe,
            permit,
            stalled_events.clone(),
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out writing IPC frame"));
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(stalled_events.receiver_count(), 0);
    }

    #[tokio::test]
    async fn subscription_stream_filters_and_releases_on_idle_disconnect() {
        let slots = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&slots).acquire_owned().await.unwrap();
        let (event_tx, rx) = broadcast::channel(2);
        let (server_pipe, mut client_pipe) = tokio::io::duplex(64);

        let stream = tokio::spawn(stream_subscription(
            server_pipe,
            rx,
            vec![EVENT_PAUSED.into()],
            permit,
        ));
        event_tx.send(IpcEvent::Resumed).unwrap();
        event_tx.send(IpcEvent::Paused).unwrap();

        let payload =
            read_frame_with_timeout(&mut client_pipe, std::time::Duration::from_millis(100))
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(
            serde_json::from_slice::<IpcEvent>(&payload).unwrap(),
            IpcEvent::Paused
        ));

        drop(client_pipe);
        tokio::time::timeout(std::time::Duration::from_millis(100), stream)
            .await
            .expect("idle disconnect must stop subscription")
            .unwrap()
            .unwrap();
        assert_eq!(event_tx.receiver_count(), 0);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn next_reports_closed_runtime_channel() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let server = server_with_swap_sender(tx);

        let response = server.process_message(IpcMessage::Next).await;
        assert_eq!(response["success"], serde_json::json!(false));
        assert_eq!(response["error"], "runtime swap channel is closed");
    }

    #[tokio::test]
    async fn next_reports_busy_runtime_queue() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(crate::scheduler::SwapRequest {
            reason: crate::scheduler::SwapReason::Interval,
            specific: None,
        })
        .unwrap();
        let server = server_with_swap_sender(tx);

        let response = server.process_message(IpcMessage::Next).await;
        assert_eq!(response["success"], serde_json::json!(false));
        assert_eq!(response["error"], "runtime swap queue is busy");
    }

    #[tokio::test]
    async fn test_ipc_status_roundtrip() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

        // Use a unique pipe name for the test to avoid clashing with a running daemon.
        const TEST_PIPE: &str = r"\\.\pipe\aurora_test_status";

        // Spin up a minimal server that handles one message then exits.
        let server_task = tokio::spawn(async move {
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(TEST_PIPE)
                .expect("create test pipe");

            server.connect().await.expect("connect");
            let mut s = server;
            let payload = read_frame(&mut s)
                .await
                .expect("read frame")
                .expect("client frame");
            let msg: IpcMessage = serde_json::from_slice(&payload).expect("parse");
            let response = match msg {
                IpcMessage::Status => {
                    serde_json::json!({"success": true, "result": {"running": true}})
                }
                _ => serde_json::json!({"success": false}),
            };
            write_frame(&mut s, &serde_json::to_vec(&response).unwrap())
                .await
                .expect("write frame");
            s.shutdown().await.ok();
        });

        // Give the server a moment to create the pipe.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Client connects and sends Status.
        let mut client = ClientOptions::new()
            .open(TEST_PIPE)
            .expect("open client pipe");

        let msg = IpcMessage::Status;
        write_frame(&mut client, &serde_json::to_vec(&msg).unwrap())
            .await
            .expect("client write");

        let response_buf = read_frame(&mut client)
            .await
            .expect("client read frame")
            .expect("response frame");

        let response: serde_json::Value =
            serde_json::from_slice(&response_buf).expect("parse response");

        assert_eq!(response["success"], serde_json::json!(true));
        assert_eq!(response["result"]["running"], serde_json::json!(true));

        let _ = server_task.await;
    }
}

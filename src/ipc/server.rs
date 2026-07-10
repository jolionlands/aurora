use anyhow::Result;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use super::messages::{IpcEvent, IpcMessage};
use super::{read_frame, write_frame, PIPE_PATH};
use crate::runtime::RuntimeHandle;

// ---------------------------------------------------------------------------
// IpcServer
// ---------------------------------------------------------------------------

pub struct IpcServer {
    event_tx: broadcast::Sender<IpcEvent>,
    shutdown_tx: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    runtime: parking_lot::Mutex<Option<RuntimeHandle>>,
}

impl IpcServer {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            event_tx,
            shutdown_tx: parking_lot::Mutex::new(None),
            runtime: parking_lot::Mutex::new(None),
        }
    }

    /// Wire a RuntimeHandle so IPC commands can drive the runtime.
    pub fn set_runtime(&self, handle: RuntimeHandle) {
        *self.runtime.lock() = Some(handle);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<IpcEvent> {
        self.event_tx.subscribe()
    }

    pub fn broadcast_event(&self, event: IpcEvent) {
        let _ = self.event_tx.send(event);
    }

    pub fn set_shutdown_sender(&self, tx: tokio::sync::oneshot::Sender<()>) {
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
            PIPE_PATH
        );

        const MAX_RETRIES: u32 = 5;
        let mut retry_count = 0u32;
        let mut first_instance = true;

        loop {
            let create_result = create_pipe_with_sa(first_instance);
            let server = match create_result {
                Ok(s) => {
                    retry_count = 0;
                    first_instance = false;
                    s
                }
                Err(e) if first_instance => match create_pipe_with_sa(false) {
                    Ok(s) => {
                        retry_count = 0;
                        first_instance = false;
                        s
                    }
                    Err(e2) => {
                        retry_count += 1;
                        warn!(
                            "Pipe create failed ({}/{}): {} / {}",
                            retry_count, MAX_RETRIES, e, e2
                        );
                        if retry_count >= MAX_RETRIES {
                            error!(
                                "IPC pipe creation failed {} times — giving up.",
                                MAX_RETRIES
                            );
                            return Err(anyhow::anyhow!("Cannot create IPC pipe: {} / {}", e, e2));
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                },
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
            };

            if let Err(e) = server.connect().await {
                warn!("Pipe connect error: {}", e);
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                continue;
            }

            info!("IPC client connected");
            let ipc = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(e) = ipc.handle_client(server).await {
                    debug!("IPC client handler error: {}", e);
                }
            });
        }
    }

    async fn handle_client(
        &self,
        mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let payload = match read_frame(&mut pipe).await? {
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
                let _ = write_frame(&mut pipe, &bytes).await;
                let _ = pipe.shutdown().await;
                return Ok(());
            }
        };

        debug!("IPC received: {:?}", message);

        // SubscribeEvents: ack, then stream events until client disconnects.
        if let IpcMessage::SubscribeEvents { ref types } = message {
            info!("IPC: SubscribeEvents {:?}", types);
            let type_filter = types.clone();
            let ack =
                serde_json::to_vec(&serde_json::json!({"success": true, "subscription_id": 1}))?;
            write_frame(&mut pipe, &ack).await?;

            let mut rx = self.event_tx.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if !type_filter.is_empty()
                            && !type_filter.iter().any(|wanted| wanted == event_type(&ev))
                        {
                            continue;
                        }
                        let bytes = match serde_json::to_vec(&ev) {
                            Ok(b) => b,
                            Err(e) => {
                                warn!("IPC: serialize event: {}", e);
                                continue;
                            }
                        };
                        if write_frame(&mut pipe, &bytes).await.is_err() {
                            debug!("IPC event subscriber disconnected");
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("IPC event subscriber lagged by {} events", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!("IPC event broadcast channel closed");
                        return Ok(());
                    }
                }
            }
        }

        let response = self.process_message(message);
        let bytes = serde_json::to_vec(&response)?;
        write_frame(&mut pipe, &bytes).await?;
        let _ = pipe.shutdown().await;
        Ok(())
    }

    fn process_message(&self, message: IpcMessage) -> serde_json::Value {
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
                    Some(h) => h.status(),
                    None => serde_json::json!({ "running": true }),
                };
                serde_json::json!({ "success": true, "result": result })
            }

            IpcMessage::Reload => {
                info!("IPC: Reload config requested");
                if let Some(handle) = self.runtime.lock().clone() {
                    match handle.reload_from_disk() {
                        Ok(()) => {
                            self.broadcast_event(IpcEvent::ConfigReloaded);
                            serde_json::json!({
                                "success": true,
                                "result": {
                                    "index_reloaded": true,
                                    "restart_required": ["schedule", "transitions", "monitors", "cache"]
                                }
                            })
                        }
                        Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                    }
                } else {
                    serde_json::json!({ "success": false, "error": "runtime not initialised" })
                }
            }

            IpcMessage::Quit => {
                info!("IPC: Quit requested");
                if let Some(tx) = self.shutdown_tx.lock().take() {
                    let _ = tx.send(());
                } else {
                    warn!("IPC Quit: no shutdown channel — calling process::exit");
                    std::process::exit(0);
                }
                serde_json::json!({ "success": true })
            }

            IpcMessage::Next => {
                let h = runtime!();
                h.skip_next();
                serde_json::json!({ "success": true })
            }

            IpcMessage::Prev => {
                let h = runtime!();
                match h.prev() {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

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
                let h = runtime!();
                match h.set_specific(std::path::PathBuf::from(path)) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::SetFolder { path } => {
                if let Some(handle) = self.runtime.lock().clone() {
                    match handle.set_folder(std::path::PathBuf::from(path)) {
                        Ok(()) => serde_json::json!({ "success": true }),
                        Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                    }
                } else {
                    serde_json::json!({ "success": false, "error": "runtime not initialised" })
                }
            }

            IpcMessage::Rate { stars } => {
                let h = runtime!();
                h.rate(stars)
            }

            IpcMessage::Ban { hash } => {
                let h = runtime!();
                h.ban(&hash)
            }

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

            IpcMessage::PlaylistCreate { name } => {
                let h = runtime!();
                match h.playlist_create(&name) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistAdd { name, path } => {
                let h = runtime!();
                match h.playlist_add(&name, &path) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistTag {
                name,
                path,
                kind,
                tags,
            } => {
                let h = runtime!();
                match h.playlist_tag(&name, &path, &kind, tags) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistRate { name, path, rating } => {
                let h = runtime!();
                match h.playlist_rate(&name, &path, rating) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistFrequency {
                name,
                path,
                frequency,
            } => {
                let h = runtime!();
                match h.playlist_frequency(&name, &path, frequency) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistRemove { name, path } => {
                let h = runtime!();
                match h.playlist_remove(&name, &path) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistActivate { name } => {
                let h = runtime!();
                match h.playlist_activate(&name) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistDeactivate => {
                let h = runtime!();
                match h.playlist_deactivate() {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }

            IpcMessage::PlaylistDelete { name } => {
                let h = runtime!();
                match h.playlist_delete(&name) {
                    Ok(()) => serde_json::json!({ "success": true }),
                    Err(e) => serde_json::json!({ "success": false, "error": e.to_string() }),
                }
            }
        }
    }
}

fn event_type(event: &IpcEvent) -> &'static str {
    match event {
        IpcEvent::Swapped { .. } => "swapped",
        IpcEvent::Paused => "paused",
        IpcEvent::Resumed => "resumed",
        IpcEvent::ConfigReloaded => "config_reloaded",
        IpcEvent::WallpaperChanged { .. } => "wallpaper_changed",
    }
}

impl Default for IpcServer {
    fn default() -> Self {
        Self::new()
    }
}

// RuntimeHandle is also imported at the top of this file. Callers outside the
// module can reach it via `aurora::runtime::RuntimeHandle` since the runtime
// module is `pub` from lib.rs.

// ---------------------------------------------------------------------------
// Pipe creation with ACL (Admins + Owner only)
// ---------------------------------------------------------------------------

fn create_pipe_with_sa(
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
            .create_with_security_attributes_raw(PIPE_PATH, sa_ptr)?
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

    #[test]
    fn event_type_matches_wire_names() {
        assert_eq!(event_type(&IpcEvent::Paused), "paused");
        assert_eq!(
            event_type(&IpcEvent::WallpaperChanged {
                monitor_id: "m".into(),
                path: "p".into(),
            }),
            "wallpaper_changed"
        );
    }

    #[test]
    fn pipe_security_attributes_build_and_drop() {
        let attrs = build_pipe_security_attributes().expect("build pipe ACL");
        assert!(!attrs.raw.lpSecurityDescriptor.is_null());
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

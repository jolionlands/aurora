use anyhow::Result;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use super::messages::{IpcEvent, IpcMessage};
use super::PIPE_PATH;
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut buffer = vec![0u8; 65536];

        loop {
            let n = match pipe.read(&mut buffer).await {
                Ok(0) => {
                    info!("IPC client disconnected");
                    return Ok(());
                }
                Ok(n) => n,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        info!("IPC client disconnected (broken pipe)");
                        return Ok(());
                    }
                    return Err(e.into());
                }
            };

            let message: IpcMessage = match serde_json::from_slice(&buffer[..n]) {
                Ok(m) => m,
                Err(e) => {
                    warn!("IPC: Failed to parse message: {}", e);
                    let err = serde_json::json!({"success": false, "error": format!("Invalid message: {}", e)});
                    let _ = pipe.write_all(&serde_json::to_vec(&err)?).await;
                    continue;
                }
            };

            debug!("IPC received: {:?}", message);

            // SubscribeEvents: ack, then stream events until client disconnects.
            if let IpcMessage::SubscribeEvents { ref types } = message {
                info!("IPC: SubscribeEvents {:?}", types);
                let ack = serde_json::to_vec(
                    &serde_json::json!({"success": true, "subscription_id": 1}),
                )?;
                let _ = pipe.write_all(&ack).await;

                let mut rx = self.event_tx.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let bytes = match serde_json::to_vec(&ev) {
                                Ok(b) => b,
                                Err(e) => {
                                    warn!("IPC: serialize event: {}", e);
                                    continue;
                                }
                            };
                            if pipe.write_all(&bytes).await.is_err() {
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
            let _ = pipe.write_all(&bytes).await;
            let _ = pipe.flush().await;
            let _ = pipe.shutdown().await;
            return Ok(());
        }
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
                            serde_json::json!({ "success": true })
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
                h.set_specific(std::path::PathBuf::from(path));
                serde_json::json!({ "success": true })
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
        }
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

    let sa_opt = match build_pipe_security_attributes() {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(
                "Failed to build pipe security attributes: {} — using defaults",
                e
            );
            None
        }
    };
    let sa_ptr: *mut std::ffi::c_void = match &sa_opt {
        Some(sa) => sa as *const _ as *mut std::ffi::c_void,
        None => std::ptr::null_mut(),
    };

    // SAFETY: sa_ptr is either null or points to local stack that outlives this
    // synchronous call. The SA is dropped before any .await in the caller.
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first_instance)
            .create_with_security_attributes_raw(PIPE_PATH, sa_ptr)?
    };
    let _ = sa_opt; // keep alive until after create
    Ok(server)
}

pub fn build_pipe_security_attributes(
) -> Result<windows::Win32::Security::SECURITY_ATTRIBUTES, anyhow::Error> {
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

    Ok(windows::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ipc_status_roundtrip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
            let mut buf = vec![0u8; 4096];
            let n = {
                let mut s = server;
                let n = s.read(&mut buf).await.expect("read");
                let msg: IpcMessage = serde_json::from_slice(&buf[..n]).expect("parse");
                let response = match msg {
                    IpcMessage::Status => {
                        serde_json::json!({"success": true, "result": {"running": true}})
                    }
                    _ => serde_json::json!({"success": false}),
                };
                s.write_all(&serde_json::to_vec(&response).unwrap())
                    .await
                    .expect("write");
                s.flush().await.expect("flush");
                s.shutdown().await.ok();
                n
            };
            n
        });

        // Give the server a moment to create the pipe.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Client connects and sends Status.
        let mut client = ClientOptions::new()
            .open(TEST_PIPE)
            .expect("open client pipe");

        let msg = IpcMessage::Status;
        client
            .write_all(&serde_json::to_vec(&msg).unwrap())
            .await
            .expect("client write");

        let mut response_buf = Vec::new();
        client
            .read_to_end(&mut response_buf)
            .await
            .expect("client read");

        let response: serde_json::Value =
            serde_json::from_slice(&response_buf).expect("parse response");

        assert_eq!(response["success"], serde_json::json!(true));
        assert_eq!(response["result"]["running"], serde_json::json!(true));

        let _ = server_task.await;
    }
}

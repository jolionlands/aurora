use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "aurora-ctl",
    about = "Control a running aurora daemon",
    version
)]
struct Cli {
    /// Output raw JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show current daemon status.
    Status,

    /// Skip to the next photo.
    Next,

    /// Restore the previous photo.
    Prev,

    /// Pause automatic cycling.
    Pause {
        /// Pause duration, e.g. "1h", "30m", "3600s".
        /// Omit for indefinite pause.
        #[arg(long)]
        duration: Option<String>,
    },

    /// Resume automatic cycling.
    Resume,

    /// Apply a specific photo immediately.
    Set {
        /// Absolute or relative path to an image file.
        path: String,
    },

    /// Narrow the active source to a specific folder for this session.
    Folder {
        /// Path to the folder.
        path: String,
    },

    /// Rate the currently displayed photo (1–5 stars).
    Rate {
        /// Star rating between 1 and 5.
        stars: u8,
    },

    /// Ban a photo by its content hash so it is never shown again.
    Ban {
        /// SHA-256 hash of the image to ban.
        hash: String,
    },

    /// Show metrics / statistics dump.
    Stats,

    /// Subscribe to a real-time event stream (newline-delimited JSON).
    Events {
        /// Event types to subscribe to (omit for all).
        #[arg(long)]
        types: Vec<String>,
    },

    /// Reload configuration from disk.
    Reload,

    /// Show which photo is currently displayed on each monitor.
    CurrentWallpaper,

    /// Gracefully stop the aurora daemon.
    Quit,

    /// Manage wallpaper playlists.
    Playlist {
        #[command(subcommand)]
        action: PlaylistCommand,
    },
}

#[derive(Subcommand)]
enum PlaylistCommand {
    /// Print all playlists and which one is active.
    List,

    /// Create an empty playlist.
    Create {
        /// Name of the new playlist.
        name: String,
    },

    /// Add a path to a playlist.
    ///
    /// Use the special value "current" to add whatever wallpaper is currently
    /// displayed.
    Add {
        /// Name of the playlist.
        name: String,
        /// Relative or absolute path, or "current".
        path: String,
    },

    /// Remove a path from a playlist.
    Remove {
        /// Name of the playlist.
        name: String,
        /// Path to remove (must match exactly what was added).
        path: String,
    },

    /// Set the active playlist and immediately apply one of its wallpapers.
    Activate {
        /// Name of the playlist to activate.
        name: String,
    },

    /// Clear the active playlist (return to full-index rotation).
    Deactivate,

    /// Delete a playlist entirely.
    Delete {
        /// Name of the playlist to delete.
        name: String,
    },
}

// ---------------------------------------------------------------------------
// Duration parsing helper: "1h" → 3600, "30m" → 1800, "3600s" / "3600" → 3600
// ---------------------------------------------------------------------------

fn parse_duration_secs(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration string");
    }
    // Check suffix
    if let Some(val) = s.strip_suffix('h') {
        let h: u64 = val
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid hours: {}", val))?;
        return Ok(h * 3600);
    }
    if let Some(val) = s.strip_suffix('m') {
        let m: u64 = val
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid minutes: {}", val))?;
        return Ok(m * 60);
    }
    if let Some(val) = s.strip_suffix('s') {
        let sec: u64 = val
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid seconds: {}", val))?;
        return Ok(sec);
    }
    // Plain number = seconds
    s.parse::<u64>()
        .map_err(|_| anyhow::anyhow!("cannot parse duration {:?}", s))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Map CLI subcommand to IPC message
    use aurora::ipc::{send_message, IpcMessage};

    match cli.command {
        Command::Status => {
            let resp = send_message(&IpcMessage::Status).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Next => {
            let resp = send_message(&IpcMessage::Next).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Prev => {
            let resp = send_message(&IpcMessage::Prev).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Pause { duration } => {
            let duration_secs = match duration {
                Some(d) => Some(parse_duration_secs(&d)?),
                None => None,
            };
            let resp = send_message(&IpcMessage::Pause { duration_secs }).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Resume => {
            let resp = send_message(&IpcMessage::Resume).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Set { path } => {
            let resp = send_message(&IpcMessage::Set { path }).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Folder { path } => {
            let resp = send_message(&IpcMessage::SetFolder { path }).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Rate { stars } => {
            if !(1..=5).contains(&stars) {
                bail!("star rating must be between 1 and 5, got {}", stars);
            }
            let resp = send_message(&IpcMessage::Rate { stars }).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Ban { hash } => {
            let resp = send_message(&IpcMessage::Ban { hash }).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Stats => {
            let resp = send_message(&IpcMessage::Stats).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Events { types } => {
            // For the events subcommand we keep the connection open and stream.
            stream_events(types).await?;
        }

        Command::Reload => {
            let resp = send_message(&IpcMessage::Reload).await?;
            print_response(&resp, cli.json)?;
        }

        Command::CurrentWallpaper => {
            let resp = send_message(&IpcMessage::GetCurrentWallpaper).await?;
            if cli.json {
                print_response(&resp, true)?;
            } else {
                // Human-readable: one line per monitor
                let v: serde_json::Value = serde_json::from_slice(&resp).unwrap_or_else(
                    |_| serde_json::json!({"success": false, "error": "bad response"}),
                );
                let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
                if success {
                    if let Some(result) = v.get("result").and_then(|r| r.as_object()) {
                        let mut monitors: Vec<(&String, &serde_json::Value)> =
                            result.iter().collect();
                        monitors.sort_by_key(|(k, _)| k.as_str());
                        for (monitor, path) in monitors {
                            let path_str = path.as_str().unwrap_or("");
                            println!("{:<30}  {}", monitor, path_str);
                        }
                    } else {
                        println!("(no wallpaper data)");
                    }
                } else {
                    let error = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("unknown error");
                    eprintln!("error: {}", error);
                    std::process::exit(1);
                }
            }
        }

        Command::Quit => {
            let resp = send_message(&IpcMessage::Quit).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Playlist { action } => {
            handle_playlist(action, cli.json).await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Playlist subcommand handler
// ---------------------------------------------------------------------------

async fn handle_playlist(action: PlaylistCommand, as_json: bool) -> anyhow::Result<()> {
    use aurora::ipc::{send_message, IpcMessage};

    match action {
        PlaylistCommand::List => {
            let resp = send_message(&IpcMessage::PlaylistList).await?;
            if as_json {
                print_response(&resp, true)?;
            } else {
                let v: serde_json::Value = serde_json::from_slice(&resp)
                    .unwrap_or_else(|_| serde_json::json!({"success": false, "error": "bad response"}));
                let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
                if success {
                    let result = v.get("result").unwrap_or(&serde_json::Value::Null);
                    let active = result.get("active").and_then(|a| a.as_str());
                    let playlists = result
                        .get("playlists")
                        .and_then(|p| p.as_array())
                        .map(|a| a.as_slice())
                        .unwrap_or(&[]);
                    if playlists.is_empty() {
                        println!("(no playlists)");
                    } else {
                        for pl in playlists {
                            let name = pl.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                            let shuffle = pl.get("shuffle").and_then(|s| s.as_bool()).unwrap_or(false);
                            let count = pl
                                .get("paths")
                                .and_then(|p| p.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let marker = if active == Some(name) { " [active]" } else { "" };
                            println!(
                                "{}{} — {} file(s), shuffle: {}",
                                name, marker, count, shuffle
                            );
                        }
                    }
                } else {
                    let error = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error");
                    eprintln!("error: {}", error);
                    std::process::exit(1);
                }
            }
        }

        PlaylistCommand::Create { name } => {
            let resp = send_message(&IpcMessage::PlaylistCreate { name }).await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Add { name, path } => {
            // Resolve "current" to the actual wallpaper via a GetCurrentWallpaper round-trip.
            let resolved_path = if path.eq_ignore_ascii_case("current") {
                let resp = send_message(&IpcMessage::GetCurrentWallpaper).await?;
                let v: serde_json::Value = serde_json::from_slice(&resp)
                    .map_err(|e| anyhow::anyhow!("bad response from daemon: {}", e))?;
                let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
                if !success {
                    let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                    anyhow::bail!("could not get current wallpaper: {}", err);
                }
                // Pick the first monitor's path.
                v.get("result")
                    .and_then(|r| r.as_object())
                    .and_then(|m| m.values().next())
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow::anyhow!("no current wallpaper reported by daemon"))?
            } else {
                path
            };

            let resp = send_message(&IpcMessage::PlaylistAdd {
                name,
                path: resolved_path,
            })
            .await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Remove { name, path } => {
            let resp = send_message(&IpcMessage::PlaylistRemove { name, path }).await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Activate { name } => {
            let resp = send_message(&IpcMessage::PlaylistActivate { name }).await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Deactivate => {
            let resp = send_message(&IpcMessage::PlaylistDeactivate).await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Delete { name } => {
            let resp = send_message(&IpcMessage::PlaylistDelete { name }).await?;
            print_response(&resp, as_json)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn print_response(bytes: &[u8], as_json: bool) -> Result<()> {
    if bytes.is_empty() {
        if as_json {
            println!("{{}}");
        } else {
            println!("(empty response)");
        }
        return Ok(());
    }

    if as_json {
        // Pretty-print the raw JSON
        let v: Value = serde_json::from_slice(bytes)?;
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    // Human-readable mode: inspect success field and result/error.
    let v: Value = serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()));

    let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);

    if success {
        if let Some(result) = v.get("result") {
            if result.is_object() || result.is_array() {
                println!("{}", serde_json::to_string_pretty(result)?);
            } else {
                println!("{}", result);
            }
        } else {
            println!("ok");
        }
    } else {
        let error = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        eprintln!("error: {}", error);
        std::process::exit(1);
    }

    Ok(())
}

/// Keep the pipe open and print each event as a JSON line.
async fn stream_events(types: Vec<String>) -> Result<()> {
    use aurora::ipc::{IpcMessage, PIPE_PATH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut client = ClientOptions::new()
        .open(PIPE_PATH)
        .map_err(|e| anyhow::anyhow!("Cannot connect to aurora daemon: {}", e))?;

    let msg = IpcMessage::SubscribeEvents { types };
    client.write_all(&serde_json::to_vec(&msg)?).await?;
    client.flush().await?;

    // First message: subscription acknowledgement
    let mut buf = vec![0u8; 4096];
    let n = client.read(&mut buf).await?;
    if n == 0 {
        bail!("Daemon disconnected immediately");
    }

    // Print subsequent events as newline-delimited JSON until disconnected.
    loop {
        let n = client.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        // Events may be concatenated; split on `}{` boundaries naively.
        // For v1 each event fits in one read.
        let line = String::from_utf8_lossy(&buf[..n]);
        println!("{}", line.trim());
    }

    Ok(())
}

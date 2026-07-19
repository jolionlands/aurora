use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

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

    /// Ban a photo by its content hash so it is never shown again.
    Ban {
        /// Full-file BLAKE3 hash of the image to ban.
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

    /// Reload sources (restart for schedule/transition/monitor/cache/metrics/log changes).
    Reload,

    /// Show the last successfully applied wallpaper snapshot for each monitor.
    CurrentWallpaper,

    /// Gracefully stop the aurora daemon.
    Quit,

    /// Manage wallpaper playlists.
    Playlist {
        #[command(subcommand)]
        action: PlaylistCommand,
    },

    /// Ask a vision model to tag one wallpaper.
    Autotag {
        /// Absolute/relative image path, or "current".
        path: String,

        /// Required Pylon/OpenAI-compatible base URL. HTTPS is required unless --allow-http.
        #[arg(long, value_name = "URL")]
        base_url: String,

        /// Allow plaintext HTTP for an explicitly trusted endpoint.
        #[arg(long)]
        allow_http: bool,

        /// Vision model to use.
        #[arg(long, default_value = "pylon-vega-gemma4")]
        model: String,

        /// Environment variable containing the API key.
        #[arg(long, default_value = "PYLON_KEY")]
        api_key_env: String,

        /// User-protected file containing the API key. Overrides --api-key-env.
        #[arg(long)]
        api_key_file: Option<String>,

        /// Request timeout in seconds.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,

        /// Create or update this playlist with returned metadata for the path.
        #[arg(long)]
        apply_playlist: Option<String>,
    },

    /// Tag many wallpapers and append a JSONL audit trail.
    AutotagBatch {
        /// Folder to scan. Mutually exclusive with --manifest.
        #[arg(required_unless_present = "manifest", conflicts_with = "manifest")]
        root: Option<String>,

        /// JSON manifest containing a rows array. Mutually exclusive with ROOT.
        #[arg(long, required_unless_present = "root", conflicts_with = "root")]
        manifest: Option<String>,

        /// Playlist to create/update with tags.
        #[arg(long, default_value = "autotagged")]
        playlist: String,

        /// Maximum number of images to tag in this run.
        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// Append-only JSONL audit file. Defaults to %APPDATA%\aurora\autotag-batch.jsonl.
        #[arg(long)]
        resume_file: Option<String>,

        /// Retag paths even when the playlist already has metadata.
        #[arg(long)]
        force: bool,

        /// Include exact duplicate image files.
        #[arg(long)]
        include_duplicates: bool,

        /// Include images below 640x360.
        #[arg(long)]
        include_small: bool,

        /// Required Pylon/OpenAI-compatible base URL. HTTPS is required unless --allow-http.
        #[arg(long, value_name = "URL")]
        base_url: String,

        /// Allow plaintext HTTP for an explicitly trusted endpoint.
        #[arg(long)]
        allow_http: bool,

        /// Vision model to use.
        #[arg(long, default_value = "pylon-vega-gemma4")]
        model: String,

        /// Environment variable containing the API key.
        #[arg(long, default_value = "PYLON_KEY")]
        api_key_env: String,

        /// User-protected file containing the API key. Overrides --api-key-env.
        #[arg(long)]
        api_key_file: Option<String>,

        /// Request timeout in seconds.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
    },
}

#[derive(Subcommand)]
enum PlaylistCommand {
    /// Print all playlists and which one is active.
    List,

    /// Show one page of paths and metadata from a playlist.
    Show {
        /// Name of the playlist.
        name: String,
        /// Zero-based path offset.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Number of paths to return (1-256).
        #[arg(
            long,
            default_value_t = aurora::ipc::messages::DEFAULT_PLAYLIST_SHOW_LIMIT,
            value_parser = parse_playlist_show_limit
        )]
        limit: usize,
    },

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

    /// Replace tags on a playlist path.
    Tag {
        /// Name of the playlist.
        name: String,
        /// Relative or absolute path, or "current".
        path: String,
        /// Built-in tag group, or any custom group name.
        #[arg(long, default_value = "general")]
        kind: String,
        /// Tags to assign; omit all tags to clear this group for the path.
        tags: Vec<String>,
    },

    /// Rate a playlist path from 0 to 5.
    Rate {
        /// Name of the playlist.
        name: String,
        /// Relative or absolute path, or "current".
        path: String,
        /// Star rating between 0 and 5.
        rating: u8,
    },

    /// Set how frequently a playlist path should appear.
    Frequency {
        /// Name of the playlist.
        name: String,
        /// Relative or absolute path, or "current".
        path: String,
        /// Weight. 1 is normal, 2 is twice as likely, etc.
        frequency: u32,
    },

    /// Enable or disable shuffled selection for a playlist.
    Shuffle {
        /// Name of the playlist.
        name: String,
        /// Whether shuffled selection is enabled.
        #[arg(action = clap::ArgAction::Set)]
        shuffle: bool,
    },

    /// Remove a path from a playlist.
    Remove {
        /// Name of the playlist.
        name: String,
        /// Relative or absolute path, or "current".
        path: String,
    },

    /// Set the active playlist and request an immediate wallpaper swap.
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
        return h
            .checked_mul(3600)
            .ok_or_else(|| anyhow::anyhow!("duration is too large"));
    }
    if let Some(val) = s.strip_suffix('m') {
        let m: u64 = val
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid minutes: {}", val))?;
        return m
            .checked_mul(60)
            .ok_or_else(|| anyhow::anyhow!("duration is too large"));
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

fn parse_playlist_show_limit(value: &str) -> std::result::Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| format!("invalid playlist page limit: {value}"))?;
    if !(1..=aurora::ipc::messages::MAX_PLAYLIST_SHOW_LIMIT).contains(&limit) {
        return Err(format!(
            "playlist page limit must be between 1 and {}",
            aurora::ipc::messages::MAX_PLAYLIST_SHOW_LIMIT
        ));
    }
    Ok(limit)
}

fn ipc_path_from(path: &Path, current_dir: &Path) -> Result<String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        if !current_dir.is_absolute() {
            bail!(
                "cannot resolve relative path against non-absolute current directory {}",
                current_dir.display()
            );
        }
        current_dir.join(path)
    };

    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("path cannot be sent over IPC because it is not valid UTF-8"))
}

fn command_path_for_ipc(path: &str) -> Result<String> {
    let path = Path::new(path);
    if path.is_absolute() {
        return ipc_path_from(path, Path::new(""));
    }

    let current_dir = std::env::current_dir().context("resolve path against current directory")?;
    ipc_path_from(path, &current_dir)
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
            let path = command_path_for_ipc(&path)?;
            let resp = send_message(&IpcMessage::Set { path }).await?;
            print_response(&resp, cli.json)?;
        }

        Command::Folder { path } => {
            let path = command_path_for_ipc(&path)?;
            let resp = send_message(&IpcMessage::SetFolder { path }).await?;
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
                        if result.is_empty() {
                            println!("(no successfully applied wallpaper data)");
                        } else {
                            println!("Last successfully applied wallpaper snapshot:");
                            let mut monitors: Vec<(&String, &serde_json::Value)> =
                                result.iter().collect();
                            monitors.sort_by_key(|(k, _)| k.as_str());
                            for (monitor, path) in monitors {
                                let path_str = path.as_str().unwrap_or("");
                                println!("{:<30}  {}", monitor, path_str);
                            }
                        }
                    } else {
                        println!("(no successfully applied wallpaper data)");
                    }
                } else {
                    let error = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("unknown error");
                    anyhow::bail!("{}", error);
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

        Command::Autotag {
            path,
            base_url,
            allow_http,
            model,
            api_key_env,
            api_key_file,
            timeout_secs,
            apply_playlist,
        } => {
            let resolved_path = resolve_playlist_path(path).await?;
            let result = autotag_image(AutoTagOptions {
                path: resolved_path.clone(),
                base_url,
                allow_http,
                model,
                api_key_env,
                api_key_file,
                timeout_secs,
            })
            .await?;

            if let Some(playlist) = apply_playlist {
                apply_autotags_to_playlist(&playlist, &resolved_path, &result, true, true).await?;
            }

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result.to_json())?);
            } else {
                println!("{} ({})", resolved_path, result.model);
                for (kind, tags) in &result.groups {
                    if !tags.is_empty() {
                        println!("{:<12} {}", kind, tags.join(", "));
                    }
                }
                if let Some(rating) = result.rating {
                    println!("{:<12} {}", "rating", rating);
                }
                if let Some(frequency) = result.frequency {
                    println!("{:<12} {}", "frequency", frequency);
                }
                if let Some(confidence) = result.confidence {
                    println!("{:<12} {:.2}", "confidence", confidence);
                }
            }
        }

        Command::AutotagBatch {
            root,
            manifest,
            playlist,
            limit,
            resume_file,
            force,
            include_duplicates,
            include_small,
            base_url,
            allow_http,
            model,
            api_key_env,
            api_key_file,
            timeout_secs,
        } => {
            let summary = autotag_batch(BatchAutoTagOptions {
                root,
                manifest,
                playlist,
                limit,
                resume_file,
                force,
                include_duplicates,
                include_small,
                base_url,
                allow_http,
                model,
                api_key_env,
                api_key_file,
                timeout_secs,
            })
            .await?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!(
                    "batch complete: tagged {}, skipped {}, failed {}",
                    summary["tagged"], summary["skipped"], summary["failed"]
                );
                if let Some(resume) = summary.get("resume_file").and_then(Value::as_str) {
                    println!("audit {}", resume);
                }
            }
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
                let v: serde_json::Value = serde_json::from_slice(&resp).unwrap_or_else(
                    |_| serde_json::json!({"success": false, "error": "bad response"}),
                );
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
                            let shuffle =
                                pl.get("shuffle").and_then(|s| s.as_bool()).unwrap_or(false);
                            let count = pl.get("path_count").and_then(Value::as_u64).unwrap_or(0);
                            let marker = if active == Some(name) {
                                " [active]"
                            } else {
                                ""
                            };
                            println!(
                                "{}{} - {} file(s), shuffle: {}",
                                name, marker, count, shuffle
                            );
                        }
                    }
                } else {
                    let error = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("unknown error");
                    anyhow::bail!("{}", error);
                }
            }
        }

        PlaylistCommand::Show {
            name,
            offset,
            limit,
        } => {
            let resp = send_message(&IpcMessage::PlaylistShow {
                name,
                offset,
                limit,
            })
            .await?;
            if as_json {
                print_response(&resp, true)?;
            } else {
                let response = ensure_success(&resp)?;
                let result = response
                    .get("result")
                    .ok_or_else(|| anyhow::anyhow!("daemon returned invalid playlist page"))?;
                let playlist = result
                    .get("playlist")
                    .ok_or_else(|| anyhow::anyhow!("daemon returned invalid playlist summary"))?;
                let playlist_name = playlist.get("name").and_then(Value::as_str).unwrap_or("?");
                let marker = if playlist.get("active").and_then(Value::as_bool) == Some(true) {
                    " [active]"
                } else {
                    ""
                };
                let shuffle = playlist
                    .get("shuffle")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let total = result.get("total").and_then(Value::as_u64).unwrap_or(0);
                let page_offset = result.get("offset").and_then(Value::as_u64).unwrap_or(0);
                let items = result
                    .get("items")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);

                println!(
                    "{}{} - {} file(s), shuffle: {}",
                    playlist_name, marker, total, shuffle
                );
                println!("offset {}: {} item(s)", page_offset, items.len());
                for item in items {
                    println!(
                        "{}",
                        item.get("path").and_then(Value::as_str).unwrap_or("?")
                    );
                    if let Some(groups) = item.get("tag_groups").and_then(Value::as_object) {
                        for (kind, tags) in groups {
                            let tags: Vec<&str> = tags
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .collect();
                            if !tags.is_empty() {
                                println!("  {}: {}", kind, tags.join(", "));
                            }
                        }
                    }
                    if let Some(rating) = item.get("rating").and_then(Value::as_u64) {
                        println!("  rating: {}", rating);
                    }
                    if let Some(frequency) = item.get("frequency").and_then(Value::as_u64) {
                        println!("  frequency: {}", frequency);
                    }
                }
                if let Some(next_offset) = result.get("next_offset").and_then(Value::as_u64) {
                    println!("next offset: {}", next_offset);
                }
            }
        }

        PlaylistCommand::Create { name } => {
            let resp = send_message(&IpcMessage::PlaylistCreate { name }).await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Add { name, path } => {
            let resolved_path = resolve_playlist_path(path).await?;

            let resp = send_message(&IpcMessage::PlaylistAdd {
                name,
                path: resolved_path,
            })
            .await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Tag {
            name,
            path,
            kind,
            tags,
        } => {
            let resolved_path = resolve_playlist_path(path).await?;
            let resp = send_message(&IpcMessage::PlaylistTag {
                name,
                path: resolved_path,
                kind,
                tags,
            })
            .await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Rate { name, path, rating } => {
            if rating > 5 {
                bail!(
                    "playlist item rating must be between 0 and 5, got {}",
                    rating
                );
            }
            let resolved_path = resolve_playlist_path(path).await?;
            let resp = send_message(&IpcMessage::PlaylistRate {
                name,
                path: resolved_path,
                rating,
            })
            .await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Frequency {
            name,
            path,
            frequency,
        } => {
            if frequency == 0 {
                bail!("playlist item frequency must be at least 1");
            }
            let resolved_path = resolve_playlist_path(path).await?;
            let resp = send_message(&IpcMessage::PlaylistFrequency {
                name,
                path: resolved_path,
                frequency,
            })
            .await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Shuffle { name, shuffle } => {
            let resp = send_message(&IpcMessage::PlaylistShuffle { name, shuffle }).await?;
            print_response(&resp, as_json)?;
        }

        PlaylistCommand::Remove { name, path } => {
            let path = resolve_playlist_path(path).await?;
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

async fn resolve_playlist_path(path: String) -> anyhow::Result<String> {
    use aurora::ipc::{send_message, IpcMessage};

    if !path.eq_ignore_ascii_case("current") {
        return absolute_playlist_path(&path);
    }

    let resp = send_message(&IpcMessage::GetCurrentWallpaper).await?;
    let v: serde_json::Value = serde_json::from_slice(&resp)
        .map_err(|e| anyhow::anyhow!("bad response from daemon: {}", e))?;
    let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
    if !success {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
        anyhow::bail!("could not get current wallpaper: {}", err);
    }
    absolute_playlist_path(&current_wallpaper_path(&v)?)
}

fn absolute_playlist_path(path: &str) -> anyhow::Result<String> {
    let path = std::path::absolute(path)
        .with_context(|| format!("make playlist path absolute: {path:?}"))?;
    ipc_path_from(&path, Path::new(""))
}

fn current_wallpaper_path(response: &Value) -> anyhow::Result<String> {
    let mut paths = response
        .get("result")
        .and_then(|r| r.as_object())
        .into_iter()
        .flat_map(|monitors| monitors.values())
        .filter_map(Value::as_str);
    let first = paths
        .next()
        .ok_or_else(|| anyhow::anyhow!("no current wallpaper reported by daemon"))?;
    if paths.any(|path| path != first) {
        anyhow::bail!(
            "monitors have different current wallpapers; pass an explicit path instead of 'current'"
        );
    }
    Ok(first.to_string())
}

struct BatchAutoTagOptions {
    root: Option<String>,
    manifest: Option<String>,
    playlist: String,
    limit: usize,
    resume_file: Option<String>,
    force: bool,
    include_duplicates: bool,
    include_small: bool,
    base_url: String,
    allow_http: bool,
    model: String,
    api_key_env: String,
    api_key_file: Option<String>,
    timeout_secs: u64,
}

#[derive(Clone, Debug)]
struct BatchCandidate {
    path: String,
    content_hash: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
}

struct BatchAuditContext<'a> {
    run_id: String,
    playlist: &'a str,
    model: &'a str,
}

const MAX_AUTOTAG_BATCH_CANDIDATES: usize = 100_000;
const MAX_AUTOTAG_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_API_KEY_FILE_BYTES: u64 = 16 * 1024;

async fn autotag_batch(opts: BatchAutoTagOptions) -> anyhow::Result<Value> {
    let resume_path = opts
        .resume_file
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            aurora::config::default_config_path().with_file_name("autotag-batch.jsonl")
        });
    let audit = BatchAuditContext {
        run_id: format!("{}-{}", current_unix_ms(), std::process::id()),
        playlist: &opts.playlist,
        model: &opts.model,
    };
    let mut candidates = collect_batch_candidates(&opts)?;
    candidates.sort_by(|a, b| a.path.cmp(&b.path));

    let mut tagged = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut attempted = 0usize;
    let mut seen_hashes = HashSet::new();
    let mut touched = Vec::new();

    for candidate in candidates {
        if !opts.include_duplicates {
            if let Some(hash) = &candidate.content_hash {
                if !seen_hashes.insert(hash.clone()) {
                    append_batch_resume(
                        &resume_path,
                        &audit,
                        "skipped-duplicate",
                        &candidate.path,
                        None,
                    )?;
                    skipped += 1;
                    continue;
                }
            }
        }
        if !opts.include_small && is_small_candidate(&candidate) {
            append_batch_resume(&resume_path, &audit, "skipped-small", &candidate.path, None)?;
            skipped += 1;
            continue;
        }
        if should_skip_tagged_candidate(
            opts.force,
            playlist_path_has_metadata(&opts.playlist, &candidate.path).await,
        )? {
            append_batch_resume(
                &resume_path,
                &audit,
                "skipped-tagged",
                &candidate.path,
                None,
            )?;
            skipped += 1;
            continue;
        }
        if !reserve_autotag_attempt(&mut attempted, opts.limit) {
            break;
        }

        match autotag_image(AutoTagOptions {
            path: candidate.path.clone(),
            base_url: opts.base_url.clone(),
            allow_http: opts.allow_http,
            model: opts.model.clone(),
            api_key_env: opts.api_key_env.clone(),
            api_key_file: opts.api_key_file.clone(),
            timeout_secs: opts.timeout_secs,
        })
        .await
        {
            Ok(result) => {
                let applied = match apply_autotags_to_playlist(
                    &opts.playlist,
                    &candidate.path,
                    &result,
                    true,
                    opts.force,
                )
                .await
                {
                    Ok(applied) => applied,
                    Err(error) => {
                        append_batch_resume(
                            &resume_path,
                            &audit,
                            "failed",
                            &candidate.path,
                            Some(json!({
                                "stage": "playlist-apply",
                                "error": error.to_string(),
                            })),
                        )?;
                        eprintln!("failed {}: {}", candidate.path, error);
                        failed += 1;
                        continue;
                    }
                };
                if !applied {
                    append_batch_resume(
                        &resume_path,
                        &audit,
                        "skipped-tagged",
                        &candidate.path,
                        None,
                    )?;
                    skipped += 1;
                    continue;
                }
                append_batch_resume(
                    &resume_path,
                    &audit,
                    "tagged",
                    &candidate.path,
                    Some(result.to_json()),
                )?;
                eprintln!(
                    "tagged {} ({}) [{} groups]",
                    candidate.path,
                    result.model,
                    result.groups.len()
                );
                tagged += 1;
                touched.push(candidate.path);
            }
            Err(e) => {
                append_batch_resume(
                    &resume_path,
                    &audit,
                    "failed",
                    &candidate.path,
                    Some(json!({
                        "error": e.to_string(),
                    })),
                )?;
                eprintln!("failed {}: {}", candidate.path, e);
                failed += 1;
            }
        }
    }

    Ok(json!({
        "playlist": opts.playlist,
        "resume_file": resume_path.display().to_string(),
        "tagged": tagged,
        "skipped": skipped,
        "failed": failed,
        "attempted": attempted,
        "touched": touched,
    }))
}

fn reserve_autotag_attempt(attempted: &mut usize, limit: usize) -> bool {
    if *attempted >= limit {
        return false;
    }
    *attempted += 1;
    true
}

fn should_skip_tagged_candidate(force: bool, status: anyhow::Result<bool>) -> anyhow::Result<bool> {
    Ok(status? && !force)
}

fn append_batch_resume(
    path: &Path,
    audit: &BatchAuditContext<'_>,
    status: &str,
    image_path: &str,
    detail: Option<Value>,
) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let record = json!({
        "ts_ms": current_unix_ms(),
        "run_id": audit.run_id,
        "playlist": audit.playlist,
        "model": audit.model,
        "status": status,
        "path": image_path,
        "detail": detail,
    });
    writeln!(file, "{}", serde_json::to_string(&record)?)?;
    Ok(())
}

fn current_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn collect_batch_candidates(opts: &BatchAutoTagOptions) -> anyhow::Result<Vec<BatchCandidate>> {
    let mut candidates = match (opts.root.as_deref(), opts.manifest.as_deref()) {
        (Some(root), None) => collect_folder_candidates(Path::new(root))?,
        (None, Some(manifest)) => collect_manifest_candidates(Path::new(manifest))?,
        (None, None) => anyhow::bail!("provide exactly one of ROOT or --manifest"),
        (Some(_), Some(_)) => anyhow::bail!("ROOT and --manifest are mutually exclusive"),
    };
    for candidate in &mut candidates {
        candidate.path = absolute_playlist_path(&candidate.path)?;
    }
    Ok(candidates)
}

fn collect_manifest_candidates(path: &Path) -> anyhow::Result<Vec<BatchCandidate>> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("read manifest metadata {}", path.display()))?
        .len();
    enforce_manifest_byte_limit(size)?;
    let file = std::fs::File::open(path)
        .with_context(|| format!("open autotag manifest {}", path.display()))?;
    let bytes = read_at_most(file, MAX_AUTOTAG_MANIFEST_BYTES)
        .with_context(|| format!("read autotag manifest {}", path.display()))?;
    enforce_manifest_byte_limit(bytes.len() as u64)?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse autotag manifest JSON {}", path.display()))?;
    let rows = value
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("manifest missing rows array: {}", path.display()))?;
    enforce_manifest_row_limit(rows.len())?;
    let mut out = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let row = row
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("manifest row {index} must be a JSON object"))?;
        let status = row
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("manifest row {index} is missing string status"))?;
        if status != "ok" {
            continue;
        }
        let path = row
            .get("absolute_path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("manifest row {index} with status ok is missing absolute_path")
            })?;
        out.push(BatchCandidate {
            path: path.to_string(),
            content_hash: manifest_sha256(row.get("sha256"), index)?,
            width: row.get("width").and_then(Value::as_u64),
            height: row.get("height").and_then(Value::as_u64),
        });
    }
    Ok(out)
}

fn manifest_sha256(value: Option<&Value>, index: usize) -> anyhow::Result<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let hash = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("manifest row {index} sha256 must be a string"))?
        .trim();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("manifest row {index} sha256 must be exactly 64 hexadecimal characters");
    }
    Ok(Some(hash.to_ascii_lowercase()))
}

fn read_at_most(reader: impl std::io::Read, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn enforce_manifest_byte_limit(size: u64) -> anyhow::Result<()> {
    if size > MAX_AUTOTAG_MANIFEST_BYTES {
        anyhow::bail!(
            "autotag manifest is {size} bytes; maximum is {MAX_AUTOTAG_MANIFEST_BYTES} bytes"
        );
    }
    Ok(())
}

fn enforce_manifest_row_limit(count: usize) -> anyhow::Result<()> {
    if count > MAX_AUTOTAG_BATCH_CANDIDATES {
        anyhow::bail!(
            "manifest contains {count} rows; autotag-batch accepts at most {MAX_AUTOTAG_BATCH_CANDIDATES}; narrow the manifest"
        );
    }
    Ok(())
}

fn collect_folder_candidates(root: &Path) -> anyhow::Result<Vec<BatchCandidate>> {
    use aurora::config::types::DEFAULT_IMAGE_EXTENSIONS;
    use aurora::index::PhotoIndex;

    let metadata = std::fs::metadata(root)
        .with_context(|| format!("read autotag folder metadata {}", root.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("autotag root is not a directory: {}", root.display());
    }
    std::fs::read_dir(root).with_context(|| format!("read autotag folder {}", root.display()))?;

    let extensions: Vec<String> = DEFAULT_IMAGE_EXTENSIONS
        .iter()
        .map(|extension| (*extension).to_string())
        .collect();
    let _com = ComApartment::initialize()?;
    let index = PhotoIndex::scan(&[root.to_path_buf()], &extensions, true)
        .map_err(|error| anyhow::anyhow!("scan autotag folder {}: {error}", root.display()))?;
    enforce_candidate_limit(index.photos.len())?;

    Ok(index
        .photos
        .into_iter()
        .map(|photo| BatchCandidate {
            path: photo.path.display().to_string(),
            content_hash: Some(photo.hash),
            width: photo.width.map(u64::from),
            height: photo.height.map(u64::from),
        })
        .collect())
}

fn enforce_candidate_limit(count: usize) -> anyhow::Result<()> {
    if count > MAX_AUTOTAG_BATCH_CANDIDATES {
        anyhow::bail!(
            "folder scan found {count} eligible images; autotag-batch accepts at most {MAX_AUTOTAG_BATCH_CANDIDATES}; narrow --root"
        );
    }
    Ok(())
}

fn is_small_candidate(candidate: &BatchCandidate) -> bool {
    matches!(
        (candidate.width, candidate.height),
        (Some(w), Some(h)) if w < 640 || h < 360
    )
}

async fn playlist_path_has_metadata(playlist: &str, path: &str) -> anyhow::Result<bool> {
    use aurora::ipc::{send_message, IpcMessage};

    let resp = send_message(&IpcMessage::PlaylistAutotagStatus {
        name: playlist.to_string(),
        path: path.to_string(),
    })
    .await?;
    let response = ensure_success(&resp)?;
    response
        .pointer("/result/has_metadata")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("daemon returned invalid playlist autotag status"))
}

struct AutoTagOptions {
    path: String,
    base_url: String,
    allow_http: bool,
    model: String,
    api_key_env: String,
    api_key_file: Option<String>,
    timeout_secs: u64,
}

struct AutoTagResult {
    path: String,
    model: String,
    groups: BTreeMap<String, Vec<String>>,
    rating: Option<u8>,
    frequency: Option<u32>,
    confidence: Option<f64>,
    raw: Value,
}

impl AutoTagResult {
    fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "model": self.model,
            "groups": self.groups,
            "rating": self.rating,
            "frequency": self.frequency,
            "confidence": self.confidence,
            "raw": self.raw,
        })
    }
}

async fn autotag_image(opts: AutoTagOptions) -> anyhow::Result<AutoTagResult> {
    let api_key = load_api_key(
        &opts.api_key_env,
        opts.api_key_file.as_deref().map(Path::new),
    )?;
    let request_url = format!("{}/chat/completions", opts.base_url.trim_end_matches('/'));
    parse_http_url(&request_url, opts.allow_http)?;
    let path = std::path::PathBuf::from(&opts.path);
    let (data_uri, palette) = prepare_autotag_image(&path)?;

    let identity = run_autotag_pass(
        "identity",
        &opts,
        &api_key,
        autotag_identity_prompt(),
        &data_uri,
    )
    .or_else(|e| {
        run_autotag_pass(
            "identity-fallback",
            &opts,
            &api_key,
            autotag_fallback_prompt(),
            &data_uri,
        )
        .map_err(|fallback| anyhow::anyhow!("{e}; fallback also failed: {fallback}"))
    })?;
    let aesthetic = run_autotag_pass(
        "aesthetic",
        &opts,
        &api_key,
        autotag_aesthetic_prompt(),
        &data_uri,
    )
    .or_else(|e| {
        run_autotag_pass(
            "aesthetic-fallback",
            &opts,
            &api_key,
            autotag_fallback_prompt(),
            &data_uri,
        )
        .map_err(|fallback| anyhow::anyhow!("{e}; fallback also failed: {fallback}"))
    })?;

    let (identity_groups, _, _, identity_confidence) = normalize_autotag_json(&identity.raw);
    let (aesthetic_groups, rating, frequency, aesthetic_confidence) =
        normalize_autotag_json(&aesthetic.raw);
    let groups = merge_tag_groups(
        merge_tag_groups(identity_groups, aesthetic_groups),
        palette.groups,
    );
    let confidence = merge_confidence(identity_confidence, aesthetic_confidence);
    let model = if identity.model == aesthetic.model {
        identity.model
    } else {
        format!("{},{}", identity.model, aesthetic.model)
    };
    let raw = json!({
        "identity": identity.raw,
        "aesthetic": aesthetic.raw,
        "color": palette.raw,
    });

    Ok(AutoTagResult {
        path: opts.path,
        model,
        groups,
        rating,
        frequency,
        confidence,
        raw,
    })
}

struct AutoTagPassResult {
    model: String,
    raw: Value,
}

struct PaletteResult {
    groups: BTreeMap<String, Vec<String>>,
    raw: Value,
}

#[derive(Default)]
struct ColorBucket {
    count: u32,
    r_sum: u64,
    g_sum: u64,
    b_sum: u64,
}

fn prepare_autotag_image(path: &Path) -> anyhow::Result<(String, PaletteResult)> {
    use base64::Engine;

    const MAX_EDGE: u32 = 1600;
    const MAX_JPEG_BYTES: usize = 12 * 1024 * 1024;
    let decoded = {
        let _com = ComApartment::initialize()?;
        aurora::decode::decode_image(path, MAX_EDGE, MAX_EDGE).map_err(|error| {
            anyhow::anyhow!("decode image {} for autotag: {error}", path.display())
        })?
    };
    let rgba = bgra_to_rgba(decoded.width, decoded.height, decoded.bgra)?;
    let image = image::RgbaImage::from_raw(decoded.width, decoded.height, rgba)
        .map(image::DynamicImage::ImageRgba8)
        .ok_or_else(|| anyhow::anyhow!("decoded autotag image has invalid dimensions"))?;
    let palette = analyze_color_palette(&image);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
        .encode_image(&image)
        .map_err(|e| anyhow::anyhow!("encode autotag thumbnail: {e}"))?;
    if jpeg.len() > MAX_JPEG_BYTES {
        anyhow::bail!("autotag thumbnail exceeds {MAX_JPEG_BYTES} byte limit");
    }
    Ok((
        format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        ),
        palette,
    ))
}

fn bgra_to_rgba(width: u32, height: u32, mut pixels: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| anyhow::anyhow!("decoded image dimensions overflow memory size"))?;
    if pixels.len() != expected {
        anyhow::bail!(
            "decoded BGRA buffer has {} bytes; expected {expected} for {width}x{height}",
            pixels.len()
        );
    }
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(pixels)
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> anyhow::Result<Self> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_err() {
            anyhow::bail!("CoInitializeEx failed before image decode: {result:?}");
        }
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

fn analyze_color_palette(image: &image::DynamicImage) -> PaletteResult {
    let thumb = image.thumbnail(96, 96).to_rgba8();
    let mut buckets: HashMap<(u8, u8, u8), ColorBucket> = HashMap::new();

    for pixel in thumb.pixels() {
        let [r, g, b, a] = pixel.0;
        if a < 64 {
            continue;
        }
        let bucket_key = (r / 32, g / 32, b / 32);
        let bucket = buckets.entry(bucket_key).or_default();
        bucket.count += 1;
        bucket.r_sum += r as u64;
        bucket.g_sum += g as u64;
        bucket.b_sum += b as u64;
    }

    let mut colors: Vec<(u32, u8, u8, u8)> = buckets
        .into_values()
        .filter(|bucket| bucket.count > 0)
        .map(|bucket| {
            (
                bucket.count,
                (bucket.r_sum / bucket.count as u64) as u8,
                (bucket.g_sum / bucket.count as u64) as u8,
                (bucket.b_sum / bucket.count as u64) as u8,
            )
        })
        .collect();
    colors.sort_by_key(|b| std::cmp::Reverse(b.0));

    let mut selected: Vec<(u32, u8, u8, u8)> = Vec::new();
    for color in colors {
        if selected
            .iter()
            .all(|(_, r, g, b)| color_distance_sq((color.1, color.2, color.3), (*r, *g, *b)) > 900)
        {
            selected.push(color);
        }
        if selected.len() >= 6 {
            break;
        }
    }

    let total: u32 = selected.iter().map(|(count, _, _, _)| *count).sum();
    let hex_colors: Vec<String> = selected
        .iter()
        .map(|(_, r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
        .collect();
    let dominant_colors: Vec<String> = selected
        .iter()
        .map(|(_, r, g, b)| named_color_tag(*r, *g, *b))
        .collect();
    let mut color_families: Vec<String> = dominant_colors
        .iter()
        .filter_map(|tag| tag.rsplit_once('-').map(|(_, family)| family.to_string()))
        .collect();
    color_families.sort();
    color_families.dedup();

    let mut groups = BTreeMap::new();
    if !hex_colors.is_empty() {
        groups.insert("hex-color".to_string(), hex_colors.clone());
    }
    if !dominant_colors.is_empty() {
        groups.insert("dominant-color".to_string(), dominant_colors.clone());
    }
    if !color_families.is_empty() {
        groups.insert("color".to_string(), color_families.clone());
    }

    let palette_json: Vec<Value> = selected
        .iter()
        .map(|(count, r, g, b)| {
            let ratio = if total == 0 {
                0.0
            } else {
                *count as f64 / total as f64
            };
            json!({
                "hex": format!("#{r:02x}{g:02x}{b:02x}"),
                "tag": named_color_tag(*r, *g, *b),
                "ratio": ratio,
            })
        })
        .collect();

    PaletteResult {
        groups,
        raw: json!({
            "palette": palette_json,
            "method": "local-quantized-thumbnail",
        }),
    }
}

fn color_distance_sq(a: (u8, u8, u8), b: (u8, u8, u8)) -> i32 {
    let dr = a.0 as i32 - b.0 as i32;
    let dg = a.1 as i32 - b.1 as i32;
    let db = a.2 as i32 - b.2 as i32;
    dr * dr + dg * dg + db * db
}

fn named_color_tag(r: u8, g: u8, b: u8) -> String {
    let rf = r as f32 / 255.0;
    let gf = g as f32 / 255.0;
    let bf = b as f32 / 255.0;
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let chroma = max - min;
    let lightness = (max + min) / 2.0;
    let tone = if lightness < 0.22 {
        "dark"
    } else if lightness > 0.78 {
        "light"
    } else {
        "mid"
    };

    let family = if lightness < 0.10 {
        "black"
    } else if lightness > 0.92 && chroma < 0.08 {
        "white"
    } else if chroma < 0.08 {
        "gray"
    } else {
        let hue = if (max - rf).abs() < f32::EPSILON {
            60.0 * (((gf - bf) / chroma) % 6.0)
        } else if (max - gf).abs() < f32::EPSILON {
            60.0 * (((bf - rf) / chroma) + 2.0)
        } else {
            60.0 * (((rf - gf) / chroma) + 4.0)
        };
        let hue = if hue < 0.0 { hue + 360.0 } else { hue };
        match hue {
            h if !(15.0..345.0).contains(&h) => "red",
            h if h < 45.0 => "orange",
            h if h < 70.0 => "yellow",
            h if h < 165.0 => "green",
            h if h < 200.0 => "cyan",
            h if h < 255.0 => "blue",
            h if h < 295.0 => "purple",
            _ => "pink",
        }
    };

    if family == "black" || family == "white" {
        family.to_string()
    } else {
        format!("{tone}-{family}")
    }
}

fn run_autotag_pass(
    label: &str,
    opts: &AutoTagOptions,
    api_key: &str,
    prompt: &str,
    data_uri: &str,
) -> anyhow::Result<AutoTagPassResult> {
    let url = format!("{}/chat/completions", opts.base_url.trim_end_matches('/'));
    let mut last_error = None;

    for attempt in 0..2 {
        let prompt_text = if attempt == 0 {
            prompt.to_string()
        } else {
            format!("{prompt}\nReturn the JSON object now. No prose, no markdown.")
        };
        let body = json!({
            "model": opts.model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": prompt_text},
                    {"type": "image_url", "image_url": {"url": data_uri}}
                ]
            }],
            "temperature": 0.0,
            "max_tokens": 900,
            "stream": false
        });

        let response = http_post_json(&url, api_key, &body, opts.timeout_secs, opts.allow_http)?;
        let returned_model = response
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let content = response
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            last_error = Some(anyhow::anyhow!("{label} pass returned empty content"));
            continue;
        }
        match parse_model_json(content) {
            Ok(raw) => {
                return Ok(AutoTagPassResult {
                    model: returned_model,
                    raw,
                });
            }
            Err(e) => {
                last_error = Some(anyhow::anyhow!("{label} pass returned invalid JSON: {e}"));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("{label} pass failed")))
}

fn autotag_identity_prompt() -> &'static str {
    r#"Analyze this wallpaper for Aurora.
Return ONLY valid minified JSON, no markdown.
Use lowercase kebab-case strings.
Return exactly this shape:
{"medium":[],"safety":[],"franchise":[],"character":[],"content":[],"source":[],"artist":[],"confidence":0.0}
Use [] when unknown and replace confidence with a 0.0..1.0 number.
medium examples: anime, real-photo, digital-painting, illustration, screenshot, 3d-render.
safety values: sfw, suggestive, lewd, nsfw.
Focus on objective identification, not taste or mood.
Identify fandom/franchise/character only when visually recognizable.
Do not guess artist/source unless visible or strongly implied."#
}

fn autotag_aesthetic_prompt() -> &'static str {
    r#"Analyze this wallpaper for Aurora.
Return ONLY valid minified JSON, no markdown.
Use lowercase kebab-case strings.
Return exactly this shape:
{"theme":[],"color":[],"style":[],"mood":[],"setting":[],"composition":[],"quality":[],"rating":0,"frequency":1,"confidence":0.0}
Use [] when unknown. Use rating 0..5, frequency >=1, confidence 0.0..1.0.
theme examples: dark-fantasy, cyberpunk, nature, minimal, cozy, abstract.
color examples: dark-tones, neon, warm, cool, monochrome, pastel.
composition examples: centered, wide-shot, close-up, landscape, portrait, symmetrical.
quality is a short tag such as clean, noisy, sharp, low-res, cluttered.
Focus on mood, palette, composition, and wallpaper usefulness.
Do not identify characters, fandoms, or safety in this pass."#
}

fn autotag_fallback_prompt() -> &'static str {
    r#"Analyze this wallpaper for Aurora.
Return ONLY valid minified JSON, no markdown.
Use lowercase kebab-case strings.
Use [] when unknown.
Top-level keys may include:
medium,safety,franchise,character,theme,content,color,source,artist,style,quality,mood,setting,composition,rating,frequency,confidence.
medium examples: anime, real-photo, digital-painting, illustration, screenshot, 3d-render.
safety values: sfw, suggestive, lewd, nsfw.
rating is an optional 0..5 integer for wallpaper quality.
frequency is an optional positive integer weight.
confidence is 0.0..1.0.
Identify fandom/franchise/character only when visually recognizable."#
}

fn parse_model_json(content: &str) -> anyhow::Result<Value> {
    let trimmed = content.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    if let Ok(value) = serde_json::from_str(unfenced) {
        return Ok(value);
    }
    let start = trimmed.find('{').ok_or_else(|| {
        anyhow::anyhow!(
            "model response did not contain JSON object: {}",
            trimmed.chars().take(240).collect::<String>()
        )
    })?;
    let end = trimmed.rfind('}').ok_or_else(|| {
        anyhow::anyhow!(
            "model response did not contain JSON object: {}",
            trimmed.chars().take(240).collect::<String>()
        )
    })?;
    serde_json::from_str(&trimmed[start..=end])
        .map_err(|e| anyhow::anyhow!("parse model JSON: {}", e))
}

#[allow(clippy::type_complexity)]
fn normalize_autotag_json(
    raw: &Value,
) -> (
    BTreeMap<String, Vec<String>>,
    Option<u8>,
    Option<u32>,
    Option<f64>,
) {
    let mut groups = BTreeMap::new();
    let mut rating = None;
    let mut frequency = None;
    let mut confidence = None;

    let Some(obj) = raw.as_object() else {
        return (groups, rating, frequency, confidence);
    };

    for (key, value) in obj {
        let kind = normalize_group_key(key);
        match kind.as_str() {
            "rating" => {
                rating = value.as_u64().map(|v| v.min(5) as u8);
            }
            "frequency" => {
                frequency = value.as_u64().map(|v| v.clamp(1, u32::MAX as u64) as u32);
            }
            "confidence" => {
                confidence = value.as_f64();
            }
            _ => {
                let tags = value_to_tags(value);
                if !tags.is_empty() {
                    groups.entry(kind).or_default().extend(tags);
                }
            }
        }
    }

    for tags in groups.values_mut() {
        tags.sort();
        tags.dedup();
    }

    (groups, rating, frequency, confidence)
}

fn merge_tag_groups(
    mut first: BTreeMap<String, Vec<String>>,
    second: BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    for (kind, tags) in second {
        first.entry(kind).or_default().extend(tags);
    }
    for tags in first.values_mut() {
        tags.sort();
        tags.dedup();
    }
    first
}

fn merge_confidence(first: Option<f64>, second: Option<f64>) -> Option<f64> {
    match (first, second) {
        (Some(a), Some(b)) => Some(((a + b) / 2.0).clamp(0.0, 1.0)),
        (Some(a), None) | (None, Some(a)) => Some(a.clamp(0.0, 1.0)),
        (None, None) => None,
    }
}

fn normalize_group_key(key: &str) -> String {
    match slug(key).as_str() {
        "source-guess" | "source-guessguess" | "collection" => "source".to_string(),
        "palette" | "colors" | "colour" | "colours" => "color".to_string(),
        "media" | "type" | "image-type" => "medium".to_string(),
        "series" | "fandom" => "franchise".to_string(),
        "characters" => "character".to_string(),
        "tags" => "general".to_string(),
        other => other.to_string(),
    }
}

fn value_to_tags(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().flat_map(value_to_tags).collect(),
        Value::String(s) => split_tags(s),
        Value::Number(n) => vec![slug(&n.to_string())],
        Value::Bool(true) => vec!["true".to_string()],
        _ => Vec::new(),
    }
}

fn split_tags(s: &str) -> Vec<String> {
    s.split([',', ';'])
        .map(slug)
        .filter(|s| !s.is_empty() && s != "unknown" && s != "none" && s != "n-a")
        .collect()
}

fn slug(s: &str) -> String {
    s.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn load_api_key(env_name: &str, file: Option<&Path>) -> anyhow::Result<String> {
    let (value, source) = if let Some(path) = file {
        let source = format!("API key file {}", path.display());
        let file = std::fs::File::open(path)
            .with_context(|| format!("open API key file {}", path.display()))?;
        let bytes = read_at_most(file, MAX_API_KEY_FILE_BYTES)
            .with_context(|| format!("read API key file {}", path.display()))?;
        if bytes.len() as u64 > MAX_API_KEY_FILE_BYTES {
            anyhow::bail!("{source} exceeds {MAX_API_KEY_FILE_BYTES}-byte limit");
        }
        let value =
            String::from_utf8(bytes).with_context(|| format!("{source} is not valid UTF-8"))?;
        (value, source)
    } else {
        (
            std::env::var(env_name).map_err(|_| {
                anyhow::anyhow!("missing API key; set {env_name} or use --api-key-file")
            })?,
            format!("environment variable {env_name}"),
        )
    };
    let key = value.trim();
    if key.is_empty() {
        anyhow::bail!("{source} is empty");
    }
    if key.chars().any(char::is_control) {
        anyhow::bail!("{source} contains a control character");
    }
    Ok(key.to_string())
}

fn http_post_json(
    url: &str,
    bearer: &str,
    body: &Value,
    timeout_secs: u64,
    allow_http: bool,
) -> anyhow::Result<Value> {
    use std::ffi::c_void;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Networking::WinHttp::{
        WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
        WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
        WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    };

    let parsed = parse_http_url(url, allow_http)?;
    let body = serde_json::to_vec(body)?;
    let body_len =
        u32::try_from(body.len()).map_err(|_| anyhow::anyhow!("request body too large"))?;
    let headers: Vec<u16> = format!(
        "Authorization: Bearer {bearer}\r\nContent-Type: application/json\r\nAccept: application/json\r\n"
    )
    .encode_utf16()
    .collect();
    let agent = HSTRING::from(concat!("aurora-ctl/", env!("CARGO_PKG_VERSION")));
    let host = HSTRING::from(&parsed.host);
    let path = HSTRING::from(&parsed.path);
    let timeout_ms = timeout_secs.saturating_mul(1000).clamp(1, i32::MAX as u64) as i32;

    unsafe {
        let session = WinHttpHandle::new(
            WinHttpOpen(
                &agent,
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            "open WinHTTP session",
        )?;
        WinHttpSetTimeouts(session.0, timeout_ms, timeout_ms, timeout_ms, timeout_ms)?;
        let connection = WinHttpHandle::new(
            WinHttpConnect(session.0, &host, parsed.port, 0),
            "connect to model gateway",
        )?;
        let flags = if parsed.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = WinHttpHandle::new(
            WinHttpOpenRequest(
                connection.0,
                windows::core::w!("POST"),
                &path,
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                flags,
            ),
            "open model request",
        )?;
        WinHttpSetOption(
            Some(request.0 as *const c_void),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes()),
        )?;
        WinHttpSendRequest(
            request.0,
            Some(&headers),
            Some(body.as_ptr().cast()),
            body_len,
            body_len,
            0,
        )?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;

        let mut status = 0u32;
        let mut status_size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut status_size,
            std::ptr::null_mut(),
        )?;

        const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
        let mut response = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            )?;
            if read == 0 {
                break;
            }
            if response.len() + read as usize > MAX_RESPONSE_BYTES {
                anyhow::bail!("model gateway response exceeds {MAX_RESPONSE_BYTES} byte limit");
            }
            response.extend_from_slice(&chunk[..read as usize]);
        }
        if !(200..300).contains(&status) {
            let text: String = String::from_utf8_lossy(&response)
                .chars()
                .take(1024)
                .collect();
            anyhow::bail!("HTTP {status} from model gateway: {text}");
        }
        serde_json::from_slice(&response).map_err(|e| anyhow::anyhow!("parse gateway JSON: {e}"))
    }
}

struct ParsedHttpUrl {
    secure: bool,
    host: String,
    port: u16,
    path: String,
}

struct WinHttpHandle(*mut std::ffi::c_void);

impl WinHttpHandle {
    fn new(raw: *mut std::ffi::c_void, operation: &str) -> anyhow::Result<Self> {
        if raw.is_null() {
            anyhow::bail!("{operation}: {}", windows::core::Error::from_win32());
        }
        Ok(Self(raw))
    }
}

impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0);
        }
    }
}

fn parse_http_url(url: &str, allow_http: bool) -> anyhow::Result<ParsedHttpUrl> {
    use windows::Win32::Networking::WinHttp::{
        WinHttpCrackUrl, ICU_REJECT_USERPWD, URL_COMPONENTS, WINHTTP_INTERNET_SCHEME_HTTP,
        WINHTTP_INTERNET_SCHEME_HTTPS,
    };

    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '#' | '\\'))
    {
        anyhow::bail!("invalid model endpoint URL");
    }

    let wide: Vec<u16> = url.encode_utf16().collect();
    let mut components = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        dwSchemeLength: u32::MAX,
        dwHostNameLength: u32::MAX,
        dwUrlPathLength: u32::MAX,
        ..Default::default()
    };
    unsafe { WinHttpCrackUrl(&wide, ICU_REJECT_USERPWD.0, &mut components) }
        .map_err(|error| anyhow::anyhow!("invalid model endpoint URL: {error}"))?;

    let secure = match components.nScheme {
        WINHTTP_INTERNET_SCHEME_HTTPS => true,
        WINHTTP_INTERNET_SCHEME_HTTP if allow_http => false,
        WINHTTP_INTERNET_SCHEME_HTTP => anyhow::bail!(
            "plaintext HTTP is disabled; use HTTPS or pass --allow-http for a trusted endpoint"
        ),
        _ => anyhow::bail!("unsupported model endpoint scheme; use https://"),
    };
    let component = |ptr: windows::core::PWSTR, len: u32| -> anyhow::Result<String> {
        let len = usize::try_from(len).context("model endpoint component is too long")?;
        if ptr.is_null() || len == 0 {
            return Ok(String::new());
        }
        String::from_utf16(unsafe { std::slice::from_raw_parts(ptr.as_ptr(), len) })
            .context("model endpoint is not valid UTF-16")
    };
    let host = component(components.lpszHostName, components.dwHostNameLength)?;
    if host.is_empty() {
        anyhow::bail!("model endpoint is missing a host");
    }
    let mut path = component(components.lpszUrlPath, components.dwUrlPathLength)?;
    if path.is_empty() {
        path.push('/');
    } else if path.starts_with('?') {
        path.insert(0, '/');
    }
    Ok(ParsedHttpUrl {
        secure,
        host,
        port: components.nPort,
        path,
    })
}

async fn apply_autotags_to_playlist(
    playlist: &str,
    path: &str,
    result: &AutoTagResult,
    create_playlist: bool,
    overwrite_existing: bool,
) -> anyhow::Result<bool> {
    use aurora::ipc::send_message;

    let response = send_message(&autotag_upsert_message(
        playlist,
        path,
        result,
        create_playlist,
        overwrite_existing,
    ))
    .await?;
    let response = ensure_success(&response)?;
    response
        .pointer("/result/applied")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("daemon returned invalid playlist autotag result"))
}

fn autotag_upsert_message(
    playlist: &str,
    path: &str,
    result: &AutoTagResult,
    create_playlist: bool,
    overwrite_existing: bool,
) -> aurora::ipc::IpcMessage {
    aurora::ipc::IpcMessage::PlaylistAutotagUpsert {
        name: playlist.to_string(),
        path: path.to_string(),
        groups: result.groups.clone(),
        rating: result.rating,
        frequency: result.frequency,
        create_playlist,
        overwrite_existing,
    }
}

fn ensure_success(resp: &[u8]) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_slice(resp)?;
    if value
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(value)
    } else {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown IPC error");
        anyhow::bail!("{}", error)
    }
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
        if v.get("success").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        anyhow::bail!(
            "{}",
            v.get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        );
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
        anyhow::bail!("{}", error);
    }

    Ok(())
}

/// Keep the pipe open and print each event as a JSON line.
async fn stream_events(types: Vec<String>) -> Result<()> {
    use aurora::ipc::{
        open_pipe_client, pipe_path, read_frame, read_frame_with_timeout, write_frame_with_timeout,
        IpcMessage, FRAME_IO_TIMEOUT,
    };

    let path = pipe_path()?;
    let mut client = open_pipe_client(&path, FRAME_IO_TIMEOUT).await?;

    let msg = IpcMessage::SubscribeEvents { types };
    write_frame_with_timeout(&mut client, &serde_json::to_vec(&msg)?, FRAME_IO_TIMEOUT).await?;

    // First message: subscription acknowledgement
    let ack = read_frame_with_timeout(&mut client, FRAME_IO_TIMEOUT)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Daemon disconnected immediately"))?;
    let ack: Value = serde_json::from_slice(&ack)?;
    if !ack.get("success").and_then(Value::as_bool).unwrap_or(false) {
        let error = ack
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("subscription failed");
        bail!("{}", error);
    }

    // Print subsequent events as one JSON object per framed message.
    loop {
        let Some(frame) = read_frame(&mut client).await? else {
            break;
        };
        let event: Value = serde_json::from_slice(&frame)?;
        println!("{}", serde_json::to_string(&event)?);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_shuffle_cli_parses_bool() {
        let cli =
            Cli::try_parse_from(["aurora-ctl", "playlist", "shuffle", "focus", "true"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Playlist {
                action: PlaylistCommand::Shuffle {
                    name,
                    shuffle: true
                }
            } if name == "focus"
        ));
    }

    #[test]
    fn playlist_show_cli_parses_and_bounds_pagination() {
        let defaults = Cli::try_parse_from(["aurora-ctl", "playlist", "show", "focus"]).unwrap();
        assert!(matches!(
            defaults.command,
            Command::Playlist {
                action: PlaylistCommand::Show {
                    name,
                    offset: 0,
                    limit: aurora::ipc::messages::DEFAULT_PLAYLIST_SHOW_LIMIT,
                }
            } if name == "focus"
        ));

        let explicit = Cli::try_parse_from([
            "aurora-ctl",
            "playlist",
            "show",
            "focus",
            "--offset",
            "12",
            "--limit",
            "256",
        ])
        .unwrap();
        assert!(matches!(
            explicit.command,
            Command::Playlist {
                action: PlaylistCommand::Show {
                    offset: 12,
                    limit: 256,
                    ..
                }
            }
        ));

        for limit in ["0", "257"] {
            let error =
                Cli::try_parse_from(["aurora-ctl", "playlist", "show", "focus", "--limit", limit])
                    .err()
                    .expect("out-of-range page limit must fail");
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        }
    }

    #[test]
    fn failed_json_response_returns_error() {
        let error =
            print_response(br#"{"success":false,"error":"playlist missing"}"#, true).unwrap_err();
        assert_eq!(error.to_string(), "playlist missing");
    }

    #[test]
    fn successful_response_is_returned_after_validation() {
        let response = ensure_success(br#"{"success":true,"result":{"applied":true}}"#).unwrap();
        assert_eq!(
            response.pointer("/result/applied"),
            Some(&Value::Bool(true))
        );
    }

    fn write_test_bmp(path: &Path, color: [u8; 3]) {
        use image::{ImageBuffer, Rgb};

        let image: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(8, 8, Rgb(color));
        image.save(path).unwrap();
    }

    #[test]
    fn duration_multiplication_rejects_overflow() {
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("3m").unwrap(), 180);
        assert!(parse_duration_secs(&format!("{}h", u64::MAX)).is_err());
        assert!(parse_duration_secs(&format!("{}m", u64::MAX)).is_err());
    }

    #[test]
    fn ipc_paths_resolve_relative_to_the_callers_directory_without_fs_lookup() {
        let current_dir = Path::new(r"C:\Users\test\wallpapers");
        let relative = Path::new(r"missing\future.jpg");

        assert_eq!(
            ipc_path_from(relative, current_dir).unwrap(),
            current_dir.join(relative).to_str().unwrap()
        );
    }

    #[test]
    fn ipc_paths_preserve_absolute_paths() {
        let absolute = Path::new(r"D:\wallpapers\photo.jpg");

        assert_eq!(
            ipc_path_from(absolute, Path::new("not-absolute")).unwrap(),
            absolute.to_str().unwrap()
        );
    }

    #[test]
    fn ipc_paths_reject_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let invalid = PathBuf::from(OsString::from_wide(&[0xD800]));
        let error = ipc_path_from(&invalid, Path::new(r"C:\wallpapers"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("not valid UTF-8"));
    }

    #[test]
    fn current_wallpaper_path_rejects_an_empty_result() {
        let error = current_wallpaper_path(&json!({ "result": {} }))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "no current wallpaper reported by daemon");
    }

    #[test]
    fn current_wallpaper_path_accepts_one_monitor() {
        assert_eq!(
            current_wallpaper_path(&json!({ "result": { "DISPLAY1": "a.jpg" } })).unwrap(),
            "a.jpg"
        );
    }

    #[test]
    fn current_wallpaper_path_accepts_the_same_path_on_two_monitors() {
        assert_eq!(
            current_wallpaper_path(&json!({
                "result": { "DISPLAY1": "same.jpg", "DISPLAY2": "same.jpg" }
            }))
            .unwrap(),
            "same.jpg"
        );
    }

    #[test]
    fn current_wallpaper_path_rejects_different_monitor_paths() {
        let error = current_wallpaper_path(&json!({
            "result": { "DISPLAY1": "a.jpg", "DISPLAY2": "b.jpg" }
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("monitors have different current wallpapers"));
        assert!(error.contains("pass an explicit path"));
    }

    #[test]
    fn playlist_paths_are_absolute_before_ipc() {
        let path = absolute_playlist_path("relative-wallpaper.jpg").unwrap();
        assert!(Path::new(&path).is_absolute());
        assert!(Path::new(&path).ends_with("relative-wallpaper.jpg"));
    }

    #[test]
    fn model_url_requires_https_or_explicit_http_opt_in() {
        let https = parse_http_url("https://example.test:8443/v1?q=1", false).unwrap();
        assert!(https.secure);
        assert_eq!(https.port, 8443);
        assert_eq!(https.path, "/v1?q=1");
        assert!(parse_http_url("http://127.0.0.1:8080/v1", false).is_err());
        assert!(
            !parse_http_url("http://127.0.0.1:8080/v1", true)
                .unwrap()
                .secure
        );
        assert!(parse_http_url("http://example.test/ok\r\nX-Evil: yes", true).is_err());
        assert!(parse_http_url("https://user@example.test/v1", false).is_err());
        assert!(parse_http_url("https://example.test/v1 bad", false).is_err());
    }

    #[test]
    fn autotag_commands_require_an_explicit_base_url() {
        for args in [
            ["aurora-ctl", "autotag", "wallpaper.jpg"],
            ["aurora-ctl", "autotag-batch", "wallpapers"],
        ] {
            let error = Cli::try_parse_from(args)
                .err()
                .expect("missing --base-url must fail");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
            assert!(error.to_string().contains("--base-url <URL>"));
        }

        assert!(Cli::try_parse_from([
            "aurora-ctl",
            "autotag",
            "wallpaper.jpg",
            "--base-url",
            "https://model.example/v1",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "aurora-ctl",
            "autotag-batch",
            "wallpapers",
            "--base-url",
            "https://model.example/v1",
        ])
        .is_ok());
    }

    #[test]
    fn autotag_batch_accepts_a_root_without_a_manifest() {
        let cli = Cli::try_parse_from([
            "aurora-ctl",
            "autotag-batch",
            "wallpapers",
            "--base-url",
            "https://model.example/v1",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::AutotagBatch {
                root: Some(root),
                manifest: None,
                ..
            } if root == "wallpapers"
        ));
    }

    #[test]
    fn autotag_batch_accepts_a_manifest_without_a_root() {
        let cli = Cli::try_parse_from([
            "aurora-ctl",
            "autotag-batch",
            "--manifest",
            "scan.json",
            "--base-url",
            "https://model.example/v1",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::AutotagBatch {
                root: None,
                manifest: Some(manifest),
                ..
            } if manifest == "scan.json"
        ));
    }

    #[test]
    fn autotag_batch_rejects_a_missing_input_source() {
        let error = Cli::try_parse_from([
            "aurora-ctl",
            "autotag-batch",
            "--base-url",
            "https://model.example/v1",
        ])
        .err()
        .expect("missing ROOT and --manifest must fail");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn autotag_batch_rejects_root_and_manifest_together() {
        let error = Cli::try_parse_from([
            "aurora-ctl",
            "autotag-batch",
            "wallpapers",
            "--manifest",
            "scan.json",
            "--base-url",
            "https://model.example/v1",
        ])
        .err()
        .expect("ROOT and --manifest together must fail");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn model_metadata_clamps_before_narrowing_integer_types() {
        let (_, rating, frequency, _) = normalize_autotag_json(&json!({
            "rating": u64::MAX,
            "frequency": u64::MAX,
        }));

        assert_eq!(rating, Some(5));
        assert_eq!(frequency, Some(u32::MAX));
    }

    #[test]
    fn api_key_file_is_trimmed_and_rejects_header_controls() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "  secret-value\r\n").unwrap();
        assert_eq!(
            load_api_key("IGNORED", Some(file.path())).unwrap(),
            "secret-value"
        );
        std::fs::write(file.path(), "secret\nvalue").unwrap();
        assert!(load_api_key("IGNORED", Some(file.path())).is_err());

        std::fs::write(file.path(), "k".repeat(MAX_API_KEY_FILE_BYTES as usize)).unwrap();
        assert_eq!(
            load_api_key("IGNORED", Some(file.path())).unwrap().len(),
            MAX_API_KEY_FILE_BYTES as usize
        );
        file.as_file().set_len(MAX_API_KEY_FILE_BYTES + 1).unwrap();
        let error = load_api_key("IGNORED", Some(file.path()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds 16384-byte limit"));
    }

    #[test]
    fn autotag_payload_is_a_bounded_jpeg_thumbnail() {
        use base64::Engine;

        let source = image::DynamicImage::new_rgb8(1700, 100);
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut encoded)
            .encode_image(&source)
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.jpg");
        std::fs::write(&path, encoded).unwrap();

        let (uri, _) = prepare_autotag_image(&path).unwrap();
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(uri.strip_prefix("data:image/jpeg;base64,").unwrap())
            .unwrap();
        let thumbnail = image::load_from_memory(&jpeg).unwrap();
        assert!(thumbnail.width() <= 1600 && thumbnail.height() <= 1600);
        assert!(jpeg.len() <= 12 * 1024 * 1024);
    }

    #[test]
    fn converts_bgra_to_rgba_without_swapping_alpha() {
        assert_eq!(
            bgra_to_rgba(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap(),
            vec![3, 2, 1, 4, 7, 6, 5, 8]
        );
        assert!(bgra_to_rgba(2, 1, vec![0; 4]).is_err());
        assert!(bgra_to_rgba(u32::MAX, u32::MAX, Vec::new()).is_err());
    }

    #[test]
    fn autotag_decode_keeps_file_and_pixel_limits() {
        let oversized = tempfile::NamedTempFile::new().unwrap();
        oversized
            .as_file()
            .set_len(aurora::decode::MAX_IMAGE_FILE_BYTES + 1)
            .unwrap();
        assert!(prepare_autotag_image(oversized.path())
            .err()
            .expect("oversized file must fail")
            .to_string()
            .contains("maximum"));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized.bmp");
        let mut bmp = vec![0u8; 54];
        bmp[0..2].copy_from_slice(b"BM");
        bmp[2..6].copy_from_slice(&54u32.to_le_bytes());
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&20_000i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&10_000i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        std::fs::write(&path, bmp).unwrap();
        assert!(prepare_autotag_image(&path)
            .err()
            .expect("oversized pixel count must fail")
            .to_string()
            .contains("pixels"));
    }

    #[test]
    fn batch_uses_bundled_default_extension_policy() {
        use aurora::config::types::DEFAULT_IMAGE_EXTENSIONS;

        for extension in [
            "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "ico",
        ] {
            assert!(DEFAULT_IMAGE_EXTENSIONS.contains(&extension));
        }
        for extension in ["avif", "heic", "heif"] {
            assert!(!DEFAULT_IMAGE_EXTENSIONS.contains(&extension));
        }
    }

    #[test]
    fn folder_scan_deduplicates_aliases_and_cycles_when_available() {
        use std::os::windows::fs::symlink_dir;

        let directory = tempfile::tempdir().unwrap();
        let actual = directory.path().join("actual");
        std::fs::create_dir(&actual).unwrap();
        write_test_bmp(&actual.join("wallpaper.bmp"), [255, 0, 0]);

        let alias_created = symlink_dir(&actual, directory.path().join("alias")).is_ok();
        let cycle_created = symlink_dir(directory.path(), actual.join("cycle")).is_ok();
        if !alias_created && !cycle_created {
            return;
        }

        let candidates = collect_folder_candidates(directory.path()).unwrap();
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn folder_scan_skips_malformed_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let valid = directory.path().join("valid.bmp");
        write_test_bmp(&valid, [255, 0, 0]);
        std::fs::write(directory.path().join("malformed.jpg"), b"not an image").unwrap();
        std::fs::File::create(directory.path().join("oversized.png"))
            .unwrap()
            .set_len(aurora::decode::MAX_IMAGE_FILE_BYTES + 1)
            .unwrap();

        let candidates = collect_folder_candidates(directory.path()).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, valid.display().to_string());
        assert_eq!(
            (candidates[0].width, candidates[0].height),
            (Some(8), Some(8))
        );
        assert!(candidates[0].content_hash.is_some());
    }

    #[test]
    fn folder_scan_rejects_missing_and_non_directory_roots() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        let error = collect_folder_candidates(&missing).unwrap_err().to_string();
        assert!(error.contains("read autotag folder metadata"));
        assert!(error.contains("missing"));

        let file = directory.path().join("file.txt");
        std::fs::write(&file, "not a directory").unwrap();
        let error = collect_folder_candidates(&file).unwrap_err().to_string();
        assert!(error.contains("autotag root is not a directory"));
    }

    #[test]
    fn folder_duplicates_supply_hashes_for_suppression() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.bmp");
        let second = directory.path().join("second.bmp");
        write_test_bmp(&first, [0, 0, 255]);
        std::fs::copy(&first, &second).unwrap();

        let candidates = collect_folder_candidates(directory.path()).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].content_hash, candidates[1].content_hash);
        assert!(candidates[0].content_hash.is_some());

        let mut seen = HashSet::new();
        let duplicates = candidates
            .iter()
            .filter(|candidate| !seen.insert(candidate.content_hash.clone().unwrap()))
            .count();
        assert_eq!(duplicates, 1);
    }

    #[test]
    fn folder_candidate_cap_fails_before_conversion() {
        assert!(enforce_candidate_limit(MAX_AUTOTAG_BATCH_CANDIDATES).is_ok());
        let error = enforce_candidate_limit(MAX_AUTOTAG_BATCH_CANDIDATES + 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("at most 100000"));
        assert!(error.contains("narrow --root"));
    }

    #[test]
    fn manifest_byte_and_row_limits_fail_before_candidate_conversion() {
        let oversized = tempfile::NamedTempFile::new().unwrap();
        oversized
            .as_file()
            .set_len(MAX_AUTOTAG_MANIFEST_BYTES + 1)
            .unwrap();
        let error = collect_manifest_candidates(oversized.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("maximum is 67108864 bytes"));

        let too_many_rows = tempfile::NamedTempFile::new().unwrap();
        let rows = std::iter::repeat_n(r#"{"status":"error"}"#, MAX_AUTOTAG_BATCH_CANDIDATES + 1)
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(too_many_rows.path(), format!(r#"{{"rows":[{rows}]}}"#)).unwrap();
        let error = collect_manifest_candidates(too_many_rows.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("manifest contains 100001 rows"));
        assert!(error.contains("at most 100000"));
    }

    #[test]
    fn manifest_reader_stops_after_the_limit_sentinel_byte() {
        let bytes = read_at_most(std::io::Cursor::new(vec![0; 32]), 8).unwrap();
        assert_eq!(bytes.len(), 9);
    }

    #[test]
    fn malformed_manifest_rows_report_the_row() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), r#"{"rows":[null]}"#).unwrap();
        let error = collect_manifest_candidates(file.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("manifest row 0 must be a JSON object"));
    }

    #[test]
    fn manifest_hashes_are_validated_and_normalized() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let uppercase = "A".repeat(64);
        std::fs::write(
            file.path(),
            format!(
                r#"{{"rows":[{{"status":"ok","absolute_path":"one.jpg","sha256":" {uppercase} "}}]}}"#
            ),
        )
        .unwrap();
        let candidates = collect_manifest_candidates(file.path()).unwrap();
        let lowercase = "a".repeat(64);
        assert_eq!(
            candidates[0].content_hash.as_deref(),
            Some(lowercase.as_str())
        );

        std::fs::write(
            file.path(),
            r#"{"rows":[{"status":"ok","absolute_path":"one.jpg","sha256":""}]}"#,
        )
        .unwrap();
        let error = collect_manifest_candidates(file.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("manifest row 0 sha256"));
        assert!(error.contains("64 hexadecimal"));
    }

    #[test]
    fn autotag_playlist_update_uses_one_targeted_message() {
        let result = AutoTagResult {
            path: "photo.jpg".to_string(),
            model: "model".to_string(),
            groups: BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
            rating: Some(4),
            frequency: Some(2),
            confidence: None,
            raw: Value::Null,
        };
        let message = autotag_upsert_message("auto", "photo.jpg", &result, true, false);
        assert!(matches!(
            message,
            aurora::ipc::IpcMessage::PlaylistAutotagUpsert { .. }
        ));
        let wire = serde_json::to_string(&message).unwrap();
        assert!(wire.contains("playlist_autotag_upsert"));
        assert!(!wire.contains("playlist_list"));
    }

    #[test]
    fn failed_attempts_consume_the_batch_limit() {
        let mut attempted = 0;
        assert!(reserve_autotag_attempt(&mut attempted, 2));
        // The first model call may fail; the second is still the final budget slot.
        assert!(reserve_autotag_attempt(&mut attempted, 2));
        assert!(!reserve_autotag_attempt(&mut attempted, 2));
        assert_eq!(attempted, 2);
    }

    #[test]
    fn forced_batch_still_requires_status_but_does_not_skip_tagged_paths() {
        assert!(should_skip_tagged_candidate(false, Ok(true)).unwrap());
        assert!(!should_skip_tagged_candidate(true, Ok(true)).unwrap());

        for force in [false, true] {
            let error = should_skip_tagged_candidate(
                force,
                Err(anyhow::anyhow!("playlist status unavailable")),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), "playlist status unavailable");
        }
    }

    #[test]
    fn batch_audit_records_include_run_attribution() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audit.jsonl");
        let audit = BatchAuditContext {
            run_id: "1234-56".to_string(),
            playlist: "focus",
            model: "vision-model",
        };

        append_batch_resume(&path, &audit, "skipped-small", "photo.jpg", None).unwrap();

        let record: Value =
            serde_json::from_str(std::fs::read_to_string(path).unwrap().trim()).unwrap();
        assert_eq!(record["run_id"], "1234-56");
        assert_eq!(record["playlist"], "focus");
        assert_eq!(record["model"], "vision-model");
    }
}

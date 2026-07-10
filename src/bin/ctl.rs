use anyhow::{bail, Result};
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

    /// Rate the currently displayed photo (1–5 stars).
    Rate {
        /// Star rating between 1 and 5.
        stars: u8,
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

    /// Reload photo sources from disk (restart for schedule/transition changes).
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

    /// Ask a vision model to tag one wallpaper.
    Autotag {
        /// Absolute/relative image path, or "current".
        path: String,

        /// Pylon/OpenAI-compatible HTTP base URL (HTTPS is not supported).
        #[arg(long, default_value = "http://100.64.0.28:8088/v1")]
        base_url: String,

        /// Vision model to use.
        #[arg(long, default_value = "pylon-vega-gemma4")]
        model: String,

        /// Environment variable containing the API key.
        #[arg(long, default_value = "PYLON_KEY")]
        api_key_env: String,

        /// API key value. Overrides --api-key-env when set.
        #[arg(long)]
        api_key: Option<String>,

        /// Request timeout in seconds.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,

        /// Persist returned tag groups to this playlist for the path.
        #[arg(long)]
        apply_playlist: Option<String>,
    },

    /// Tag many wallpapers with resume support.
    AutotagBatch {
        /// Folder to scan when --manifest is not supplied.
        root: String,

        /// Optional scan manifest JSON from aurora wallpaper scan.
        #[arg(long)]
        manifest: Option<String>,

        /// Playlist to create/update with tags.
        #[arg(long, default_value = "autotagged")]
        playlist: String,

        /// Maximum number of images to tag in this run.
        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// Resume JSONL file. Defaults to %APPDATA%\aurora\autotag-batch.jsonl.
        #[arg(long)]
        resume_file: Option<String>,

        /// Retag paths even when the playlist already has metadata.
        #[arg(long)]
        force: bool,

        /// Include exact duplicate files from the scan manifest.
        #[arg(long)]
        include_duplicates: bool,

        /// Include images below 640x360 from the scan manifest.
        #[arg(long)]
        include_small: bool,

        /// Pylon/OpenAI-compatible HTTP base URL (HTTPS is not supported).
        #[arg(long, default_value = "http://100.64.0.28:8088/v1")]
        base_url: String,

        /// Vision model to use.
        #[arg(long, default_value = "pylon-vega-gemma4")]
        model: String,

        /// Environment variable containing the API key.
        #[arg(long, default_value = "PYLON_KEY")]
        api_key_env: String,

        /// API key value. Overrides --api-key-env when set.
        #[arg(long)]
        api_key: Option<String>,

        /// Request timeout in seconds.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
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

    /// Replace tags on a playlist path.
    Tag {
        /// Name of the playlist.
        name: String,
        /// Relative or absolute path, or "current".
        path: String,
        /// Tag category: general, theme, content, color, source, medium, safety, franchise, or character.
        #[arg(long, default_value = "general")]
        kind: String,
        /// Tags to assign.
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

        Command::Autotag {
            path,
            base_url,
            model,
            api_key_env,
            api_key,
            timeout_secs,
            apply_playlist,
        } => {
            let resolved_path = resolve_playlist_path(path).await?;
            let result = autotag_image(AutoTagOptions {
                path: resolved_path.clone(),
                base_url,
                model,
                api_key_env,
                api_key,
                timeout_secs,
            })
            .await?;

            if let Some(playlist) = apply_playlist {
                apply_autotags_to_playlist(&playlist, &resolved_path, &result).await?;
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
            model,
            api_key_env,
            api_key,
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
                model,
                api_key_env,
                api_key,
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
                    println!("resume {}", resume);
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
                            let count = pl
                                .get("paths")
                                .and_then(|p| p.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let _weighted = pl
                                .get("items")
                                .and_then(|p| p.as_array())
                                .map(|items| {
                                    items
                                        .iter()
                                        .map(|item| {
                                            item.get("frequency")
                                                .and_then(|f| f.as_u64())
                                                .unwrap_or(1)
                                        })
                                        .sum::<u64>()
                                })
                                .unwrap_or(count as u64);
                            let marker = if active == Some(name) {
                                " [active]"
                            } else {
                                ""
                            };
                            println!(
                                "{}{} — {} file(s), shuffle: {}",
                                name, marker, count, shuffle
                            );
                        }
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

async fn resolve_playlist_path(path: String) -> anyhow::Result<String> {
    use aurora::ipc::{send_message, IpcMessage};

    if !path.eq_ignore_ascii_case("current") {
        return Ok(path);
    }

    let resp = send_message(&IpcMessage::GetCurrentWallpaper).await?;
    let v: serde_json::Value = serde_json::from_slice(&resp)
        .map_err(|e| anyhow::anyhow!("bad response from daemon: {}", e))?;
    let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
    if !success {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
        anyhow::bail!("could not get current wallpaper: {}", err);
    }
    v.get("result")
        .and_then(|r| r.as_object())
        .and_then(|m| m.values().next())
        .and_then(|p| p.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no current wallpaper reported by daemon"))
}

struct BatchAutoTagOptions {
    root: String,
    manifest: Option<String>,
    playlist: String,
    limit: usize,
    resume_file: Option<String>,
    force: bool,
    include_duplicates: bool,
    include_small: bool,
    base_url: String,
    model: String,
    api_key_env: String,
    api_key: Option<String>,
    timeout_secs: u64,
}

#[derive(Clone)]
struct BatchCandidate {
    path: String,
    sha256: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
}

async fn autotag_batch(opts: BatchAutoTagOptions) -> anyhow::Result<Value> {
    let resume_path = opts
        .resume_file
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(default_autotag_resume_path);
    let completed = load_batch_resume(&resume_path, !opts.force, opts.force)?;
    let playlist_snapshot = ensure_batch_playlist(&opts.playlist).await?;
    let mut candidates = collect_batch_candidates(&opts)?;
    candidates.sort_by(|a, b| a.path.cmp(&b.path));

    let mut tagged = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut seen_hashes = HashSet::new();
    let mut touched = Vec::new();

    for candidate in candidates {
        if tagged >= opts.limit {
            break;
        }
        if completed.contains(&candidate.path) {
            skipped += 1;
            continue;
        }
        if !opts.include_duplicates {
            if let Some(hash) = &candidate.sha256 {
                if !seen_hashes.insert(hash.clone()) {
                    append_batch_resume(&resume_path, "skipped-duplicate", &candidate.path, None)?;
                    skipped += 1;
                    continue;
                }
            }
        }
        if !opts.include_small && is_small_candidate(&candidate) {
            append_batch_resume(&resume_path, "skipped-small", &candidate.path, None)?;
            skipped += 1;
            continue;
        }
        if !opts.force
            && playlist_path_has_metadata(&playlist_snapshot, &opts.playlist, &candidate.path)
        {
            append_batch_resume(&resume_path, "skipped-tagged", &candidate.path, None)?;
            skipped += 1;
            continue;
        }

        match autotag_image(AutoTagOptions {
            path: candidate.path.clone(),
            base_url: opts.base_url.clone(),
            model: opts.model.clone(),
            api_key_env: opts.api_key_env.clone(),
            api_key: opts.api_key.clone(),
            timeout_secs: opts.timeout_secs,
        })
        .await
        {
            Ok(result) => {
                ensure_playlist_path(&opts.playlist, &candidate.path, &playlist_snapshot).await?;
                apply_autotags_to_playlist(&opts.playlist, &candidate.path, &result).await?;
                append_batch_resume(
                    &resume_path,
                    "tagged",
                    &candidate.path,
                    Some(result.to_json()),
                )?;
                println!(
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
        "touched": touched,
    }))
}

fn default_autotag_resume_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aurora")
        .join("autotag-batch.jsonl")
}

fn load_batch_resume(
    path: &Path,
    include_failed: bool,
    include_tagged: bool,
) -> anyhow::Result<HashSet<String>> {
    let mut done = HashSet::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(done);
    };
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        let is_done = match status {
            "skipped-small" | "skipped-duplicate" => true,
            "tagged" | "skipped-tagged" => !include_tagged,
            "failed" => include_failed,
            _ => false,
        };
        if is_done {
            if let Some(path) = value.get("path").and_then(Value::as_str) {
                done.insert(path.to_string());
            }
        }
    }
    Ok(done)
}

fn append_batch_resume(
    path: &Path,
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
    if let Some(manifest) = &opts.manifest {
        return collect_manifest_candidates(Path::new(manifest));
    }
    let mut out = Vec::new();
    collect_folder_candidates(Path::new(&opts.root), &mut out)?;
    Ok(out)
}

fn collect_manifest_candidates(path: &Path) -> anyhow::Result<Vec<BatchCandidate>> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let rows = value
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("manifest missing rows array: {}", path.display()))?;
    let mut out = Vec::new();
    for row in rows {
        if row.get("status").and_then(Value::as_str) != Some("ok") {
            continue;
        }
        let Some(path) = row.get("absolute_path").and_then(Value::as_str) else {
            continue;
        };
        out.push(BatchCandidate {
            path: path.to_string(),
            sha256: row
                .get("sha256")
                .and_then(Value::as_str)
                .map(|s| s.to_string()),
            width: row.get("width").and_then(Value::as_u64),
            height: row.get("height").and_then(Value::as_u64),
        });
    }
    Ok(out)
}

fn collect_folder_candidates(root: &Path, out: &mut Vec<BatchCandidate>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_folder_candidates(&path, out)?;
        } else if is_supported_image_path(&path) {
            let dimensions = image::image_dimensions(&path).ok();
            out.push(BatchCandidate {
                path: path.display().to_string(),
                sha256: None,
                width: dimensions.map(|(width, _)| width as u64),
                height: dimensions.map(|(_, height)| height as u64),
            });
        }
    }
    Ok(())
}

fn is_supported_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(
            "jpg"
                | "jpeg"
                | "png"
                | "webp"
                | "bmp"
                | "gif"
                | "ico"
                | "avif"
                | "tif"
                | "tiff"
                | "heic"
                | "heif",
        )
    )
}

fn is_small_candidate(candidate: &BatchCandidate) -> bool {
    matches!(
        (candidate.width, candidate.height),
        (Some(w), Some(h)) if w < 640 || h < 360
    )
}

async fn ensure_batch_playlist(name: &str) -> anyhow::Result<Value> {
    use aurora::ipc::{send_message, IpcMessage};

    let resp = send_message(&IpcMessage::PlaylistList).await?;
    ensure_success(&resp)?;
    let snapshot: Value = serde_json::from_slice(&resp)?;
    if playlist_exists(&snapshot, name) {
        return Ok(snapshot);
    }
    let create_resp = send_message(&IpcMessage::PlaylistCreate {
        name: name.to_string(),
    })
    .await?;
    ensure_success(&create_resp)?;
    let resp = send_message(&IpcMessage::PlaylistList).await?;
    ensure_success(&resp)?;
    Ok(serde_json::from_slice(&resp)?)
}

fn playlist_exists(snapshot: &Value, name: &str) -> bool {
    snapshot
        .pointer("/result/playlists")
        .and_then(Value::as_array)
        .map(|playlists| {
            playlists
                .iter()
                .any(|pl| pl.get("name").and_then(Value::as_str) == Some(name))
        })
        .unwrap_or(false)
}

fn playlist_path_has_metadata(snapshot: &Value, playlist: &str, path: &str) -> bool {
    let Some(item) = playlist_item(snapshot, playlist, path) else {
        return false;
    };
    if item.get("rating").is_some_and(|rating| !rating.is_null()) {
        return true;
    }
    item.get("tag_groups")
        .and_then(Value::as_object)
        .map(|groups| {
            groups.values().any(|tags| {
                tags.as_array()
                    .map(|items| !items.is_empty())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn playlist_item<'a>(snapshot: &'a Value, playlist: &str, path: &str) -> Option<&'a Value> {
    snapshot
        .pointer("/result/playlists")?
        .as_array()?
        .iter()
        .find(|pl| pl.get("name").and_then(Value::as_str) == Some(playlist))?
        .get("items")?
        .as_array()?
        .iter()
        .find(|item| item.get("path").and_then(Value::as_str) == Some(path))
}

async fn ensure_playlist_path(playlist: &str, path: &str, snapshot: &Value) -> anyhow::Result<()> {
    use aurora::ipc::{send_message, IpcMessage};

    if playlist_item(snapshot, playlist, path).is_some() {
        return Ok(());
    }
    let resp = send_message(&IpcMessage::PlaylistAdd {
        name: playlist.to_string(),
        path: path.to_string(),
    })
    .await?;
    ensure_success(&resp)?;
    Ok(())
}

struct AutoTagOptions {
    path: String,
    base_url: String,
    model: String,
    api_key_env: String,
    api_key: Option<String>,
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
    use base64::Engine;

    let path = std::path::PathBuf::from(&opts.path);
    aurora::decode::validate_image_file(&path)
        .map_err(|e| anyhow::anyhow!("rejecting image {}: {e}", path.display()))?;
    let bytes = std::fs::read(&path)
        .map_err(|e| anyhow::anyhow!("read image {}: {}", path.display(), e))?;
    let mime = mime_for_path(&path);
    let data_uri = format!(
        "data:{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );

    let api_key = opts
        .api_key
        .or_else(|| load_api_key(&opts.api_key_env))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "missing API key; set {}, pass --api-key, or keep it in lattice-chat/.env.local",
                opts.api_key_env
            )
        })?;

    let identity = run_autotag_pass(
        "identity",
        &opts.base_url,
        &opts.model,
        &api_key,
        opts.timeout_secs,
        autotag_identity_prompt(),
        &data_uri,
    )
    .or_else(|e| {
        run_autotag_pass(
            "identity-fallback",
            &opts.base_url,
            &opts.model,
            &api_key,
            opts.timeout_secs,
            autotag_fallback_prompt(),
            &data_uri,
        )
        .map_err(|fallback| anyhow::anyhow!("{e}; fallback also failed: {fallback}"))
    })?;
    let aesthetic = run_autotag_pass(
        "aesthetic",
        &opts.base_url,
        &opts.model,
        &api_key,
        opts.timeout_secs,
        autotag_aesthetic_prompt(),
        &data_uri,
    )
    .or_else(|e| {
        run_autotag_pass(
            "aesthetic-fallback",
            &opts.base_url,
            &opts.model,
            &api_key,
            opts.timeout_secs,
            autotag_fallback_prompt(),
            &data_uri,
        )
        .map_err(|fallback| anyhow::anyhow!("{e}; fallback also failed: {fallback}"))
    })?;

    let (identity_groups, _, _, identity_confidence) = normalize_autotag_json(&identity.raw);
    let (aesthetic_groups, rating, frequency, aesthetic_confidence) =
        normalize_autotag_json(&aesthetic.raw);
    let palette = analyze_color_palette(&bytes)?;
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

fn analyze_color_palette(bytes: &[u8]) -> anyhow::Result<PaletteResult> {
    let image = image::load_from_memory(bytes)
        .map_err(|e| anyhow::anyhow!("decode image for color palette: {e}"))?;
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

    Ok(PaletteResult {
        groups,
        raw: json!({
            "palette": palette_json,
            "method": "local-quantized-thumbnail",
        }),
    })
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
    base_url: &str,
    model: &str,
    api_key: &str,
    timeout_secs: u64,
    prompt: &str,
    data_uri: &str,
) -> anyhow::Result<AutoTagPassResult> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut last_error = None;

    for attempt in 0..2 {
        let prompt_text = if attempt == 0 {
            prompt.to_string()
        } else {
            format!("{prompt}\nReturn the JSON object now. No prose, no markdown.")
        };
        let body = json!({
            "model": model,
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

        let response = http_post_json(&url, api_key, &body, timeout_secs)?;
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
                rating = value.as_u64().map(|v| (v as u8).min(5));
            }
            "frequency" => {
                frequency = value.as_u64().map(|v| (v as u32).max(1));
            }
            "confidence" => {
                confidence = value.as_f64();
            }
            _ => {
                let tags = value_to_tags(value);
                if !tags.is_empty() {
                    groups.entry(kind).or_insert_with(Vec::new).extend(tags);
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

fn mime_for_path(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("ico") => "image/x-icon",
        Some("avif") => "image/avif",
        Some("tif") | Some("tiff") => "image/tiff",
        Some("heic") | Some("heif") => "image/heic",
        _ => "image/png",
    }
}

fn load_api_key(env_name: &str) -> Option<String> {
    if let Ok(value) = std::env::var(env_name) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let env_path = std::path::Path::new(r"C:\Users\kalli\Development\lattice-chat\.env.local");
    let content = std::fs::read_to_string(env_path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == env_name {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn http_post_json(
    url: &str,
    bearer: &str,
    body: &Value,
    timeout_secs: u64,
) -> anyhow::Result<Value> {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let parsed = parse_http_url(url)?;
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let addr = format!("{}:{}", parsed.host, parsed.port);
    // `connect` has no timeout and can hang forever on an unreachable gateway.
    // Keep connection establishment under the same bound as request I/O.
    let mut stream = addr
        .to_socket_addrs()?
        .find_map(|socket_addr| TcpStream::connect_timeout(&socket_addr, timeout).ok())
        .ok_or_else(|| anyhow::anyhow!("could not connect to model gateway {addr}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    if bearer.bytes().any(|b| b.is_ascii_control()) {
        anyhow::bail!("API key contains an HTTP header control character");
    }
    let body = serde_json::to_vec(body)?;
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        parsed.path,
        parsed.host_header(),
        bearer,
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;

    const MAX_HTTP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
    let mut raw = Vec::new();
    stream
        .take((MAX_HTTP_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut raw)?;
    if raw.len() > MAX_HTTP_RESPONSE_BYTES {
        anyhow::bail!(
            "model gateway response exceeds {} byte limit",
            MAX_HTTP_RESPONSE_BYTES
        );
    }
    let (status, headers, response_body) = split_http_response(&raw)?;
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(&response_body);
        anyhow::bail!("HTTP {} from model gateway: {}", status, text);
    }
    let body = if headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        decode_chunked(&response_body)?
    } else {
        response_body
    };
    serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("parse gateway JSON: {}", e))
}

struct ParsedHttpUrl {
    host: String,
    port: u16,
    path: String,
}

impl ParsedHttpUrl {
    fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_http_url(url: &str) -> anyhow::Result<ParsedHttpUrl> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("only http:// model endpoints are supported: {}", url))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{}", path)),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse::<u16>()?),
        None => (authority.to_string(), 80),
    };
    if host.is_empty()
        || authority
            .bytes()
            .any(|b| matches!(b, b'@' | b'?' | b'#' | b'\\'))
        || host.bytes().any(|b| b.is_ascii_control() || b == b' ')
        || path
            .bytes()
            .any(|b| b.is_ascii_control() || b == b' ' || b == b'#')
    {
        anyhow::bail!("missing host in {}", url);
    }
    Ok(ParsedHttpUrl { host, port, path })
}

fn split_http_response(raw: &[u8]) -> anyhow::Result<(u16, BTreeMap<String, String>, Vec<u8>)> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("invalid HTTP response: missing header terminator"))?;
    let header = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid HTTP response: missing status line"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("invalid HTTP response: bad status line"))?
        .parse::<u16>()?;
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok((status, headers, raw[header_end + 4..].to_vec()))
}

fn decode_chunked(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        if pos >= raw.len() {
            anyhow::bail!("invalid chunked response: missing chunk size");
        }
        let line_end = raw[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow::anyhow!("invalid chunked response"))?
            + pos;
        let line = String::from_utf8_lossy(&raw[pos..line_end]);
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)?;
        pos = line_end + 2;
        if size == 0 {
            break;
        }
        let data_end = pos
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("invalid chunked response: chunk size overflow"))?;
        if data_end > raw.len() {
            anyhow::bail!("invalid chunked response: chunk exceeds body");
        }
        out.extend_from_slice(&raw[pos..data_end]);
        let next = data_end.checked_add(2).ok_or_else(|| {
            anyhow::anyhow!("invalid chunked response: chunk terminator overflow")
        })?;
        if next > raw.len() || &raw[data_end..next] != b"\r\n" {
            anyhow::bail!("invalid chunked response: missing chunk terminator");
        }
        pos = next;
    }
    Ok(out)
}

async fn apply_autotags_to_playlist(
    playlist: &str,
    path: &str,
    result: &AutoTagResult,
) -> anyhow::Result<()> {
    use aurora::ipc::{send_message, IpcMessage};

    // Tag/rating/frequency operations require an existing playlist item. Make
    // the one-shot command behave like batch mode without duplicating entries.
    let snapshot_response = send_message(&IpcMessage::PlaylistList).await?;
    ensure_success(&snapshot_response)?;
    let snapshot: Value = serde_json::from_slice(&snapshot_response)?;
    if !playlist_exists(&snapshot, playlist) {
        anyhow::bail!("playlist '{}' not found", playlist);
    }
    ensure_playlist_path(playlist, path, &snapshot).await?;

    for (kind, tags) in &result.groups {
        if tags.is_empty() {
            continue;
        }
        let resp = send_message(&IpcMessage::PlaylistTag {
            name: playlist.to_string(),
            path: path.to_string(),
            kind: kind.clone(),
            tags: tags.clone(),
        })
        .await?;
        ensure_success(&resp)?;
    }

    if let Some(rating) = result.rating {
        let resp = send_message(&IpcMessage::PlaylistRate {
            name: playlist.to_string(),
            path: path.to_string(),
            rating,
        })
        .await?;
        ensure_success(&resp)?;
    }

    if let Some(frequency) = result.frequency {
        let resp = send_message(&IpcMessage::PlaylistFrequency {
            name: playlist.to_string(),
            path: path.to_string(),
            frequency,
        })
        .await?;
        ensure_success(&resp)?;
    }

    Ok(())
}

fn ensure_success(resp: &[u8]) -> anyhow::Result<()> {
    let value: Value = serde_json::from_slice(resp)?;
    if value
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(())
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
    use aurora::ipc::{read_frame, write_frame, IpcMessage, PIPE_PATH};
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut client = ClientOptions::new()
        .open(PIPE_PATH)
        .map_err(|e| anyhow::anyhow!("Cannot connect to aurora daemon: {}", e))?;

    let msg = IpcMessage::SubscribeEvents { types };
    write_frame(&mut client, &serde_json::to_vec(&msg)?).await?;

    // First message: subscription acknowledgement
    let ack = read_frame(&mut client)
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
    fn chunked_decoder_rejects_truncated_chunks_without_panicking() {
        assert!(decode_chunked(b"4\r\nabc").is_err());
        assert!(decode_chunked(b"ffffffffffffffff\r\n").is_err());
        assert_eq!(
            decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(),
            b"Wikipedia"
        );
    }

    #[test]
    fn http_url_rejects_header_injection_bytes() {
        assert!(parse_http_url("http://example.test/ok\r\nX-Evil: yes").is_err());
        assert!(parse_http_url("http://example.test:8080/v1").is_ok());
        assert!(parse_http_url("http://user@example.test/v1").is_err());
        assert!(parse_http_url("http://example.test/v1 bad").is_err());
    }

    #[test]
    fn force_resume_only_reprocesses_tagged_entries() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "{\"status\":\"tagged\",\"path\":\"a.jpg\"}\n{\"status\":\"skipped-small\",\"path\":\"b.jpg\"}\n{\"status\":\"failed\",\"path\":\"c.jpg\"}\n",
        )
        .unwrap();

        let normal = load_batch_resume(file.path(), true, false).unwrap();
        assert!(normal.contains("a.jpg"));
        assert!(normal.contains("b.jpg"));
        assert!(normal.contains("c.jpg"));

        let force = load_batch_resume(file.path(), false, true).unwrap();
        assert!(!force.contains("a.jpg"));
        assert!(force.contains("b.jpg"));
        assert!(!force.contains("c.jpg"));
    }
}

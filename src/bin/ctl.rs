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

    /// Gracefully stop the aurora daemon.
    Quit,
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
        let h: u64 = val.trim().parse().map_err(|_| anyhow::anyhow!("invalid hours: {}", val))?;
        return Ok(h * 3600);
    }
    if let Some(val) = s.strip_suffix('m') {
        let m: u64 = val.trim().parse().map_err(|_| anyhow::anyhow!("invalid minutes: {}", val))?;
        return Ok(m * 60);
    }
    if let Some(val) = s.strip_suffix('s') {
        let sec: u64 = val.trim().parse().map_err(|_| anyhow::anyhow!("invalid seconds: {}", val))?;
        return Ok(sec);
    }
    // Plain number = seconds
    s.parse::<u64>().map_err(|_| anyhow::anyhow!("cannot parse duration {:?}", s))
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
            if stars < 1 || stars > 5 {
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

        Command::Quit => {
            let resp = send_message(&IpcMessage::Quit).await?;
            print_response(&resp, cli.json)?;
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;
    use aurora::ipc::{IpcMessage, PIPE_PATH};

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

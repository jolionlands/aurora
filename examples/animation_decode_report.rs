//! Decode every file in a folder the way the animated-wallpaper player would,
//! and print one line per file: outcome, frames kept, stored size, decode
//! time, and the decoder's peak working set.
//!
//! ```text
//! cargo run --release --example animation_decode_report -- <folder> [WIDTHxHEIGHT]
//! ```
//! Each file is decoded in a child process so a crash or a memory spike is
//! attributed to that file alone.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use aurora::animation::frames::{decode_animation, FrameBudget};
use aurora::config::types::AnimatedConfig;

fn peak_working_set_mb() -> f64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb);
    }
    counters.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
}

fn decode_one(path: &Path, width: u32, height: u32) {
    let config = AnimatedConfig::default();
    let budget = FrameBudget {
        display_width: width,
        display_height: height,
        max_bytes: config.max_memory_mb as usize * 1024 * 1024,
        max_frames: config.max_frames as usize,
        min_delay: Duration::from_millis(1000 / u64::from(config.max_fps)),
        scaling: config.scaling,
    };
    let started = Instant::now();
    let result = decode_animation(path, &budget);
    let millis = started.elapsed().as_millis();
    let peak = peak_working_set_mb();
    match result {
        Ok(Some(animation)) => println!(
            "ANIMATED pixel_art={} frames={} size={}x{} stored_mb={:.1} loop_ms={} decode_ms={millis} peak_ws_mb={peak:.1}",
            animation.pixel_art,
            animation.frames.len(),
            animation.width,
            animation.height,
            animation.bytes() as f64 / (1024.0 * 1024.0),
            animation.loop_duration().as_millis(),
        ),
        Ok(None) => println!("STATIC decode_ms={millis} peak_ws_mb={peak:.1}"),
        Err(error) => println!(
            "REJECTED decode_ms={millis} peak_ws_mb={peak:.1} error={}",
            format!("{error:#}").replace('\n', " ")
        ),
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_else(|| ".".into());
    if first == "--one" {
        let path = PathBuf::from(args.next().expect("path"));
        let width: u32 = args.next().expect("width").parse()?;
        let height: u32 = args.next().expect("height").parse()?;
        decode_one(&path, width, height);
        return Ok(());
    }
    let (width, height) = args
        .next()
        .and_then(|size| {
            let (w, h) = size.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((2560u32, 1600u32));

    let mut files: Vec<PathBuf> = std::fs::read_dir(&first)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    let exe = std::env::current_exe()?;
    for file in files {
        let output = Command::new(&exe)
            .arg("--one")
            .arg(&file)
            .arg(width.to_string())
            .arg(height.to_string())
            .output()?;
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let line = if output.status.success() && !line.is_empty() {
            line
        } else {
            format!("CRASHED status={:?}", output.status.code())
        };
        println!(
            "{:<44} {}",
            file.file_name().unwrap_or_default().to_string_lossy(),
            line
        );
    }
    Ok(())
}

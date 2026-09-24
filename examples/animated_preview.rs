//! Preview an animated GIF/WebP on the desktop layer without the daemon.
//!
//! ```text
//! cargo run --release --example animated_preview -- <file.gif> [seconds]
//! ```
//! With no file, a synthetic animation is generated in the temp directory.
//! The static wallpaper is not changed; only the animation window is shown.

use std::path::PathBuf;
use std::time::Duration;

use aurora::animation::{AnimationPlayer, DisplayTarget};
use aurora::apply::{WallpaperApplier, WallpaperFit};
use aurora::config::types::AnimatedConfig;
use aurora::runtime::ComApartment;
use aurora::transition::Rect;

fn synthetic_gif() -> anyhow::Result<PathBuf> {
    use image::codecs::gif::{GifEncoder, Repeat};
    let path = std::env::temp_dir().join("aurora-preview.gif");
    let mut encoder = GifEncoder::new(std::fs::File::create(&path)?);
    encoder.set_repeat(Repeat::Infinite)?;
    for index in 0..24u32 {
        let image = image::RgbaImage::from_fn(320, 200, |x, y| {
            let phase = (x + y + index * 14) % 256;
            image::Rgba([phase as u8, (255 - phase) as u8, (index * 10) as u8, 255])
        });
        let delay = image::Delay::from_saturating_duration(Duration::from_millis(80));
        encoder.encode_frame(image::Frame::from_parts(image, 0, 0, delay))?;
    }
    Ok(path)
}

fn main() -> anyhow::Result<()> {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    tracing_subscriber::fmt().with_env_filter("debug").init();

    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => synthetic_gif()?,
    };
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(15);

    let monitors = {
        let _com = ComApartment::initialize()?;
        WallpaperApplier::new()?.list_monitors()?
    };
    let config = AnimatedConfig {
        enabled: true,
        pause_on_battery: false,
        pause_when_covered: false,
        ..AnimatedConfig::default()
    };
    let mut player = AnimationPlayer::from_config(&config).expect("enabled");
    for monitor in monitors {
        println!(
            "{} at {}x{}+{}+{}",
            monitor.id, monitor.width, monitor.height, monitor.x, monitor.y
        );
        player.show(
            DisplayTarget {
                monitor_id: monitor.id,
                bounds: Rect {
                    x: monitor.x,
                    y: monitor.y,
                    width: monitor.width,
                    height: monitor.height,
                },
                fit: WallpaperFit::Fill,
            },
            path.clone(),
        );
    }
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

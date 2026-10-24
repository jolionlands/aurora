//! Native KDE Plasma wallpaper commits over the session D-Bus.
//! No X11 desktop windows: Plasma owns the wallpaper surface on Wayland too.
mod common;
pub use common::*;

use crate::config::types::Config;
use anyhow::{Context, Result};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const PLUGIN_ID: &str = "io.github.jolionlands.aurora";

pub struct WallpaperApplier {
    fit: Cell<WallpaperFit>,
}

impl WallpaperApplier {
    pub fn new() -> Result<Self> {
        Ok(Self {
            fit: Cell::new(WallpaperFit::Fill),
        })
    }

    pub fn list_monitors(&self) -> Result<Vec<MonitorInfo>> {
        let output = evaluate_script(
            r#"
            var monitors = [], seen = {};
            desktops().forEach(function(d) {
                if (d.screen < 0 || seen[d.screen]) return;
                seen[d.screen] = true;
                var g = screenGeometry(d.screen);
                monitors.push({id: "plasma-screen-" + d.screen,
                    x: g.x, y: g.y, width: g.width, height: g.height});
            });
            print(JSON.stringify(monitors));
        "#,
        )?;
        serde_json::from_str(&output).context("parse Plasma monitor geometry")
    }

    pub fn current_for_monitor(&self, monitor_id: &str) -> Result<Option<PathBuf>> {
        let screen = monitor_screen(monitor_id)?;
        let output = evaluate_script(&format!(
            r#"
            var value = "";
            desktops().forEach(function(d) {{
                if (d.screen !== {screen}) return;
                d.currentConfigGroup = ["Wallpaper", d.wallpaperPlugin, "General"];
                value = d.readConfig(d.wallpaperPlugin === {plugin} ? "Source" : "Image", "");
            }});
            print(JSON.stringify(value));
        "#,
            plugin = serde_json::to_string(PLUGIN_ID)?
        ))?;
        let value: String = serde_json::from_str(&output).context("read Plasma wallpaper path")?;
        if value.is_empty() {
            return Ok(None);
        }
        if value.starts_with("file:") {
            return Ok(url::Url::parse(&value)?.to_file_path().ok());
        }
        Ok(Some(PathBuf::from(value)))
    }

    pub fn set_for_monitor(&self, monitor_id: &str, path: &Path) -> Result<()> {
        self.commit(Some(monitor_screen(monitor_id)?), path)
    }

    pub fn set_for_all_monitors(&self, path: &Path) -> Result<()> {
        self.commit(None, path)
    }

    pub fn set_fit(&self, fit: WallpaperFit) -> Result<()> {
        self.fit.set(fit);
        Ok(())
    }

    fn commit(&self, screen: Option<u32>, path: &Path) -> Result<()> {
        let path =
            std::fs::canonicalize(path).with_context(|| format!("wallpaper {}", path.display()))?;
        let dimensions = validate_wallpaper(&path)?;
        if dimensions.0 == 0 || dimensions.1 == 0 {
            anyhow::bail!("wallpaper has no displayable frames");
        }
        install_wallpaper_plugin()?;
        let config_path = crate::config::parse::default_config_path();
        let config = if config_path.is_file() {
            crate::config::parse::parse_kdl_config(&std::fs::read_to_string(config_path)?)?
        } else {
            Config::default()
        };
        let source = url::Url::from_file_path(&path)
            .map_err(|_| anyhow::anyhow!("wallpaper path must be absolute"))?;
        let kind = if is_video(&path) {
            "video"
        } else if config.animated.enabled && crate::animation::may_be_animated(&path) {
            "animated"
        } else {
            "image"
        };
        let filter = screen
            .map(|s| format!("d.screen === {s}"))
            .unwrap_or_else(|| "d.screen >= 0".into());
        let fit = screen
            .and_then(|screen| {
                let id = format!("plasma-screen-{screen}");
                config
                    .monitors
                    .iter()
                    .find(|monitor| monitor.name == id)
                    .map(|monitor| WallpaperFit::parse(&monitor.fit))
            })
            .unwrap_or(self.fit.get());
        let output = evaluate_script(&format!(
            r#"
            var changed = 0;
            desktops().forEach(function(d) {{
                if (!({filter})) return;
                d.wallpaperPlugin = {plugin};
                d.currentConfigGroup = ["Wallpaper", {plugin}, "General"];
                d.writeConfig("Source", {source});
                d.writeConfig("Kind", {kind});
                d.writeConfig("FillMode", {fill});
                d.writeConfig("Muted", {muted});
                d.writeConfig("Volume", {volume});
                d.writeConfig("PauseOnBattery", {battery});
                d.writeConfig("PauseWhenCovered", {covered});
                d.writeConfig("PlaybackPaused", false);
                changed++;
            }});
            print(changed);
        "#,
            plugin = serde_json::to_string(PLUGIN_ID)?,
            source = serde_json::to_string(source.as_str())?,
            kind = serde_json::to_string(kind)?,
            fill = fill_mode(fit),
            muted = config.video.muted,
            volume = config.video.volume,
            battery = config.animated.pause_on_battery,
            covered = config.animated.pause_when_covered,
        ))?;
        if output.parse::<u32>().unwrap_or(0) == 0 {
            anyhow::bail!("no attached Plasma desktop matched the requested monitor");
        }
        Ok(())
    }
}

fn monitor_screen(id: &str) -> Result<u32> {
    id.strip_prefix("plasma-screen-")
        .ok_or_else(|| anyhow::anyhow!("invalid Plasma monitor {id}"))?
        .parse()
        .context("invalid Plasma screen number")
}

fn fill_mode(fit: WallpaperFit) -> u32 {
    match fit {
        WallpaperFit::Stretch => 0,
        WallpaperFit::Contain => 1,
        WallpaperFit::Fill | WallpaperFit::Span => 2,
        WallpaperFit::Tile => 3,
        WallpaperFit::Center => 6,
    }
}

pub fn configured_global_fit(config: &Config, monitors: &[MonitorInfo]) -> WallpaperFit {
    monitors
        .first()
        .and_then(|m| config.monitors.iter().find(|o| o.name == m.id))
        .map(|o| WallpaperFit::parse(&o.fit))
        .unwrap_or(WallpaperFit::Fill)
}

pub fn evaluate_script(script: &str) -> Result<String> {
    dbus(
        "org.kde.plasmashell",
        "/PlasmaShell",
        "org.kde.PlasmaShell.evaluateScript",
        &[script],
    )
}

fn dbus(service: &str, object: &str, method: &str, args: &[&str]) -> Result<String> {
    let child = Command::new("qdbus6")
        .args([service, object, method])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start qdbus6 (Aurora Linux requires KDE Plasma 6 and Qt D-Bus tools)")?;
    let output = crate::linux::wait_for_child(child, Duration::from_secs(5))?;
    if !output.status.success() {
        anyhow::bail!(
            "KDE D-Bus request failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value = String::from_utf8(output.stdout).context("KDE D-Bus returned invalid UTF-8")?;
    Ok(value.trim().to_owned())
}

pub fn session_idle_secs() -> Result<u64> {
    dbus(
        "org.freedesktop.ScreenSaver",
        "/ScreenSaver",
        "org.freedesktop.ScreenSaver.GetSessionIdleTime",
        &[],
    )?
    .parse()
    .map_err(Into::into)
}

pub fn current_workspace() -> Result<u32> {
    dbus("org.kde.KWin", "/KWin", "org.kde.KWin.currentDesktop", &[])?
        .parse()
        .map_err(Into::into)
}

/// Freeze playback while the session is locked. Power and covered-window
/// policy live in QML so they also work when the daemon is being restarted.
pub async fn maintain_playback_policy() {
    loop {
        let result = tokio::task::spawn_blocking(|| -> Result<()> {
            let locked = dbus(
                "org.freedesktop.ScreenSaver",
                "/ScreenSaver",
                "org.freedesktop.ScreenSaver.GetActive",
                &[],
            )?;
            let locked = locked.trim() == "true";
            evaluate_script(&format!(
                r#"
                desktops().forEach(function(d) {{
                    if (d.screen < 0 || d.wallpaperPlugin !== {plugin}) return;
                    d.currentConfigGroup = ["Wallpaper", {plugin}, "General"];
                    if (d.readConfig("PlaybackPaused", false) !== {locked})
                        d.writeConfig("PlaybackPaused", {locked});
                }});
            "#,
                plugin = serde_json::to_string(PLUGIN_ID)?
            ))?;
            Ok(())
        })
        .await;
        if let Ok(Err(error)) = result {
            tracing::debug!(%error, "KDE playback policy unavailable");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub fn install_wallpaper_plugin() -> Result<PathBuf> {
    let directory = crate::linux::data_home()
        .join("plasma/wallpapers")
        .join(PLUGIN_ID);
    for (name, data) in [
        (
            "metadata.json",
            include_str!("../../resources/plasma/metadata.json"),
        ),
        (
            "contents/config/main.xml",
            include_str!("../../resources/plasma/contents/config/main.xml"),
        ),
        (
            "contents/ui/main.qml",
            include_str!("../../resources/plasma/contents/ui/main.qml"),
        ),
        (
            "contents/ui/config.qml",
            include_str!("../../resources/plasma/contents/ui/config.qml"),
        ),
    ] {
        let destination = directory.join(name);
        if std::fs::read_to_string(&destination).ok().as_deref() != Some(data) {
            std::fs::create_dir_all(destination.parent().unwrap())?;
            std::fs::write(&destination, data)
                .with_context(|| format!("install {}", destination.display()))?;
        }
    }
    Ok(directory)
}

pub fn is_video(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "mp4" | "m4v" | "webm" | "mkv" | "mov" | "avi"
        )
    })
}

pub fn validate_wallpaper(path: &Path) -> Result<(u32, u32)> {
    if !is_video(path) {
        return crate::decode::validate_image_file(path);
    }
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() || meta.len() > MAX_VIDEO_FILE_BYTES {
        anyhow::bail!("video must be a regular file smaller than 8 GiB");
    }
    let child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start ffprobe (install FFmpeg to use video wallpapers)")?;
    let output = crate::linux::wait_for_child(child, Duration::from_secs(5))?;
    if !output.status.success() {
        anyhow::bail!(
            "invalid video {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let stream = &value["streams"][0];
    let width = stream["width"].as_u64().unwrap_or(0);
    let height = stream["height"].as_u64().unwrap_or(0);
    if width == 0 || height == 0 || width.saturating_mul(height) > crate::decode::MAX_IMAGE_PIXELS {
        anyhow::bail!("video has missing or oversized frame dimensions");
    }
    Ok((width as u32, height as u32))
}

pub const MAX_VIDEO_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn screen_names_and_fit_modes_match_plasma() {
        assert_eq!(monitor_screen("plasma-screen-2").unwrap(), 2);
        assert!(monitor_screen("bad").is_err());
        assert_eq!(fill_mode(WallpaperFit::Fill), 2);
        assert_eq!(fill_mode(WallpaperFit::Contain), 1);
        assert_eq!(fill_mode(WallpaperFit::Center), 6);
    }
    #[test]
    fn wallpaper_url_roundtrips_special_characters_without_script_injection() {
        let path = Path::new("/tmp/it's a \"picture\" #1%.mp4");
        let url = url::Url::from_file_path(path).unwrap();
        assert_eq!(url.to_file_path().unwrap(), path);
        assert!(url.as_str().contains("%23"));
        assert!(url.as_str().contains("%25"));
        let encoded = serde_json::to_string(url.as_str()).unwrap();
        assert_eq!(
            serde_json::from_str::<String>(&encoded).unwrap(),
            url.as_str()
        );
    }
    #[test]
    fn malformed_video_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.mp4");
        std::fs::write(&path, b"not a video").unwrap();
        assert!(validate_wallpaper(&path).is_err());
    }

    #[test]
    fn video_is_validated_and_indexed_for_rotation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.mp4");
        let child = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=320x240:r=1",
                "-t",
                "1",
                "-c:v",
                "mpeg4",
                "-threads",
                "1",
            ])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("start FFmpeg test fixture: {error}"),
        };
        let output = crate::linux::wait_for_child(child, Duration::from_secs(5)).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(validate_wallpaper(&path).unwrap(), (320, 240));
        let source = crate::config::types::SourceConfig {
            path: directory.path().to_owned(),
            extensions: vec!["mp4".into()],
            min_width: 0,
            min_height: 0,
            ..Default::default()
        };
        let index = crate::index::PhotoIndex::scan_sources(&[source]).unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index.photos[0].path, path);
        assert_eq!(index.photos[0].width, Some(320));
        assert_eq!(index.photos[0].hash.len(), 64);
    }
}

//! Freedesktop autostart registration for the current user.
use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct StartupManager;

impl StartupManager {
    pub fn register(&self) -> Result<()> {
        let path = autostart_path();
        let executable = std::env::current_exe().context("locate Aurora executable")?;
        let command = quote_exec(&executable.to_string_lossy());
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, format!(
            "[Desktop Entry]\nType=Application\nName=Aurora\nComment=Desktop wallpaper rotation\nExec={command}\nTerminal=false\nX-KDE-autostart-after=panel\n"
        )).with_context(|| format!("write autostart {}", path.display()))
    }

    pub fn unregister(&self) -> Result<()> {
        match std::fs::remove_file(autostart_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn is_registered(&self) -> bool {
        autostart_path().is_file()
    }

    pub fn get_registered_path(&self) -> Option<String> {
        std::fs::read_to_string(autostart_path())
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("Exec=").map(str::to_owned))
    }
}

fn autostart_path() -> PathBuf {
    crate::linux::config_home().join("autostart/aurora.desktop")
}

fn quote_exec(path: &str) -> String {
    // Desktop-entry escaping is separate from shell escaping. Percent is an
    // Exec field code and must be doubled even inside double quotes.
    let escaped = path
        .replace('\\', "\\\\\\\\")
        .replace('"', "\\\\\"")
        .replace('`', "\\\\`")
        .replace('$', "\\\\$")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn autostart_exec_preserves_spaces_and_field_codes() {
        assert_eq!(
            quote_exec("/tmp/My Aurora/aurora"),
            "\"/tmp/My Aurora/aurora\""
        );
        assert_eq!(quote_exec("/tmp/100%/aurora"), "\"/tmp/100%%/aurora\"");
    }
}

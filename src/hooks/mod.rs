use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use windows::core::{Owned, PCWSTR};
use windows::Win32::Foundation::ERROR_MORE_DATA;
use windows::Win32::System::Registry::{
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_READ, KEY_WRITE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};

pub struct StartupManager {
    exe_path: PathBuf,
}

impl Default for StartupManager {
    fn default() -> Self {
        let exe_path = std::env::current_exe().unwrap_or_default();

        Self { exe_path }
    }
}

impl StartupManager {
    pub fn is_registered(&self) -> bool {
        self.get_registered_command()
            .is_some_and(|command| registration_matches(&command, self.exe_path.as_os_str()))
    }

    pub fn get_registered_path(&self) -> Option<String> {
        self.get_registered_command()
            .map(|command| String::from_utf16_lossy(&command))
    }

    fn get_registered_command(&self) -> Option<Vec<u16>> {
        unsafe {
            let hkey = open_run_key(KEY_READ).ok()?;
            let value_name = windows::core::w!("aurora");

            let mut value_type = REG_VALUE_TYPE::default();
            let mut data_size = 0;
            let size_result = RegQueryValueExW(
                *hkey,
                value_name,
                None,
                Some(&mut value_type),
                None,
                Some(&mut data_size),
            );

            if size_result.is_ok() && value_type == REG_SZ {
                read_registry_string(*hkey, value_name, data_size)
            } else {
                None
            }
        }
    }

    pub fn register(&self) -> Result<()> {
        unsafe {
            let hkey = open_run_key(KEY_WRITE).context("open Windows Run key")?;
            let path_bytes: Vec<u8> = quoted_executable_command(self.exe_path.as_os_str())
                .into_iter()
                .chain(std::iter::once(0))
                .flat_map(|c| c.to_le_bytes())
                .collect();

            RegSetValueExW(
                *hkey,
                windows::core::w!("aurora"),
                0,
                REG_SZ,
                Some(&path_bytes),
            )
            .ok()
            .context("set Aurora Windows Run value")?;
        }

        Ok(())
    }

    pub fn unregister(&self) -> Result<()> {
        unsafe {
            let hkey = open_run_key(KEY_WRITE).context("open Windows Run key")?;
            RegDeleteValueW(*hkey, windows::core::w!("aurora"))
                .ok()
                .context("delete Aurora Windows Run value")?;
        }

        Ok(())
    }
}

fn open_run_key(access: REG_SAM_FLAGS) -> windows::core::Result<Owned<HKEY>> {
    let mut hkey = HKEY::default();
    unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            0,
            access,
            &mut hkey,
        )
        .ok()?;
        Ok(Owned::new(hkey))
    }
}

fn quoted_executable_command(path: &OsStr) -> Vec<u16> {
    std::iter::once('"' as u16)
        .chain(path.encode_wide())
        .chain(std::iter::once('"' as u16))
        .collect()
}

fn registration_matches(command: &[u16], path: &OsStr) -> bool {
    command == quoted_executable_command(path)
}

unsafe fn read_registry_string(
    hkey: HKEY,
    value_name: PCWSTR,
    mut data_size: u32,
) -> Option<Vec<u16>> {
    loop {
        let mut data = vec![0u8; data_size as usize];
        let mut actual_size = data_size;
        let mut value_type = REG_VALUE_TYPE::default();
        let result = RegQueryValueExW(
            hkey,
            value_name,
            None,
            Some(&mut value_type),
            Some(data.as_mut_ptr()),
            Some(&mut actual_size),
        );

        if result == ERROR_MORE_DATA {
            data_size = actual_size.max(data_size.saturating_mul(2)).max(2);
            continue;
        }
        if result.is_err()
            || value_type != REG_SZ
            || actual_size as usize > data.len()
            || !actual_size.is_multiple_of(2)
        {
            return None;
        }

        data.truncate(actual_size as usize);
        let mut command: Vec<u16> = data
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        while command.last() == Some(&0) {
            command.pop();
        }
        return (!command.contains(&0)).then_some(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn quotes_executable_command() {
        let path = OsStr::new(r"C:\Program Files\Aurora\aurora.exe");
        assert_eq!(
            quoted_executable_command(path),
            r#""C:\Program Files\Aurora\aurora.exe""#
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn registration_requires_the_exact_quoted_executable() {
        let path = OsStr::new(r"C:\Aurora\aurora.exe");
        assert!(registration_matches(&quoted_executable_command(path), path));
        assert!(!registration_matches(
            &r"C:\Aurora\aurora.exe".encode_utf16().collect::<Vec<_>>(),
            path
        ));
        assert!(!registration_matches(
            &r#""C:\Old\aurora.exe""#.encode_utf16().collect::<Vec<_>>(),
            path
        ));
    }

    #[test]
    fn quoting_preserves_non_unicode_path_units() {
        let path_units = [b'C' as u16, b':' as u16, b'\\' as u16, 0xD800];
        let path = OsString::from_wide(&path_units);
        let mut expected = vec!['"' as u16];
        expected.extend(path_units);
        expected.push('"' as u16);

        assert_eq!(quoted_executable_command(&path), expected);
    }
}

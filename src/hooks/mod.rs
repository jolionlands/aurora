use anyhow::Result;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use tracing::warn;
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_MORE_DATA;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, REG_VALUE_TYPE,
};

const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub struct StartupManager {
    app_name: String,
    exe_path: PathBuf,
}

impl StartupManager {
    pub fn new() -> Self {
        let exe_path = std::env::current_exe().unwrap_or_default();

        Self {
            app_name: "aurora".to_string(),
            exe_path,
        }
    }

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
            let mut hkey: HKEY = HKEY::default();
            let key_path: Vec<u16> = OsStr::new(RUN_KEY_PATH)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let result = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(key_path.as_ptr()),
                0,
                KEY_READ,
                &mut hkey,
            );

            if result.is_err() {
                return None;
            }

            let value_name: Vec<u16> = OsStr::new(&self.app_name)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let mut value_type = REG_VALUE_TYPE::default();
            let mut data_size = 0;
            let size_result = RegQueryValueExW(
                hkey,
                PCWSTR::from_raw(value_name.as_ptr()),
                None,
                Some(&mut value_type),
                None,
                Some(&mut data_size),
            );

            let command = if size_result.is_ok() && value_type == REG_SZ {
                read_registry_string(hkey, &value_name, data_size)
            } else {
                None
            };

            if let Err(e) = RegCloseKey(hkey).ok() {
                warn!(
                    "StartupManager::get_registered_path: RegCloseKey failed: {:?}",
                    e
                );
            }

            command
        }
    }

    pub fn register(&self) -> Result<()> {
        unsafe {
            let mut hkey: HKEY = HKEY::default();
            let key_path: Vec<u16> = OsStr::new(RUN_KEY_PATH)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let result = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(key_path.as_ptr()),
                0,
                KEY_WRITE,
                &mut hkey,
            );
            if result.is_err() {
                return Err(anyhow::anyhow!(
                    "Failed to open Run registry key: {:?}",
                    result
                ));
            }

            let value_name: Vec<u16> = OsStr::new(&self.app_name)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let path_bytes: Vec<u8> = quoted_executable_command(self.exe_path.as_os_str())
                .into_iter()
                .chain(std::iter::once(0))
                .flat_map(|c| c.to_le_bytes())
                .collect();

            let result = RegSetValueExW(
                hkey,
                PCWSTR::from_raw(value_name.as_ptr()),
                0,
                REG_SZ,
                Some(&path_bytes),
            );
            if result.is_err() {
                let _ = RegCloseKey(hkey);
                return Err(anyhow::anyhow!(
                    "Failed to set registry value: {:?}",
                    result
                ));
            }

            let result = RegCloseKey(hkey);
            if result.is_err() {
                return Err(anyhow::anyhow!(
                    "Failed to close registry key: {:?}",
                    result
                ));
            }
        }

        Ok(())
    }

    pub fn unregister(&self) -> Result<()> {
        unsafe {
            let mut hkey: HKEY = HKEY::default();
            let key_path: Vec<u16> = OsStr::new(RUN_KEY_PATH)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let result = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(key_path.as_ptr()),
                0,
                KEY_WRITE,
                &mut hkey,
            );
            if result.is_err() {
                return Err(anyhow::anyhow!(
                    "Failed to open Run registry key: {:?}",
                    result
                ));
            }

            let value_name: Vec<u16> = OsStr::new(&self.app_name)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let result = RegDeleteValueW(hkey, PCWSTR::from_raw(value_name.as_ptr()));
            if result.is_err() {
                let _ = RegCloseKey(hkey);
                return Err(anyhow::anyhow!(
                    "Failed to delete registry value: {:?}",
                    result
                ));
            }

            let result = RegCloseKey(hkey);
            if result.is_err() {
                return Err(anyhow::anyhow!(
                    "Failed to close registry key: {:?}",
                    result
                ));
            }
        }

        Ok(())
    }
}

impl Default for StartupManager {
    fn default() -> Self {
        Self::new()
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
    value_name: &[u16],
    mut data_size: u32,
) -> Option<Vec<u16>> {
    loop {
        let mut data = vec![0u8; data_size as usize];
        let mut actual_size = data_size;
        let mut value_type = REG_VALUE_TYPE::default();
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(value_name.as_ptr()),
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

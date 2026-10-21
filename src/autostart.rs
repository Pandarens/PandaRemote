//! Start Panda when the user logs in: a per-user `Run` entry (no admin rights needed).

use anyhow::Result;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "PandaRemote";

/// Command line registered for autostart, started minimized: the installed copy in Program Files when
/// there is one (it is what the service runs), otherwise this executable.
fn command() -> Result<String> {
    let installed = std::env::var_os("ProgramFiles")
        .map(|p| std::path::PathBuf::from(p).join("PandaRC").join("panda-rc.exe"))
        .filter(|p| p.exists());
    let exe = match installed {
        Some(p) => p,
        None => std::env::current_exe()?,
    };
    Ok(format!("\"{}\" --minimized", exe.display()))
}

#[cfg(windows)]
pub fn set(enabled: bool) -> Result<()> {
    use windows::{
        Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
            RegDeleteValueW, RegSetValueExW,
        },
        core::{HSTRING, PCWSTR},
    };
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
        let result = if enabled {
            let data: Vec<u8> = command()?.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
            RegSetValueExW(key, &HSTRING::from(VALUE), None, REG_SZ, Some(&data)).ok()
        } else {
            // Already absent is fine.
            let _ = RegDeleteValueW(key, &HSTRING::from(VALUE));
            Ok(())
        };
        let _ = RegCloseKey(key);
        Ok(result?)
    }
}

#[cfg(not(windows))]
pub fn set(_enabled: bool) -> Result<()> {
    Ok(())
}

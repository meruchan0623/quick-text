//! Login item: HKCU Run key on Windows, a LaunchAgent plist on macOS.
#[cfg(target_os = "macos")]
use std::{fs, path::PathBuf};

pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .and_then(|r| r.get_value::<String, _>("QuickerText"))
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        mac_login_path().map(|p| p.exists()).unwrap_or(false)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}
pub fn set_enabled(enabled: bool) -> Result<bool, String> {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        let run = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                KEY_SET_VALUE,
            )
            .map_err(|e| e.to_string())?;
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            run.set_value(
                "QuickerText",
                &format!("\"{}\" --background", exe.display()),
            )
            .map_err(|e| e.to_string())?
        } else {
            match run.delete_value("QuickerText") {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(enabled)
    }
    #[cfg(target_os = "macos")]
    {
        set_mac_autostart(enabled)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = enabled;
        Err("Login item support is unavailable on this platform".into())
    }
}
#[cfg(target_os = "macos")]
fn mac_login_path() -> Result<PathBuf, String> {
    let user_home = std::env::var_os("HOME").ok_or("macOS home directory is unavailable")?;
    Ok(PathBuf::from(user_home).join("Library/LaunchAgents/com.quickertext.desktop.plist"))
}
#[cfg(target_os = "macos")]
fn set_mac_autostart(enabled: bool) -> Result<bool, String> {
    let path = mac_login_path()?;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let escaped = exe
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let content=format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>com.quickertext.desktop</string><key>ProgramArguments</key><array><string>{escaped}</string><string>--background</string></array><key>RunAtLoad</key><true/></dict></plist>");
        fs::create_dir_all(path.parent().ok_or("Login item parent unavailable")?)
            .map_err(|e| e.to_string())?;
        fs::write(path, content).map_err(|e| e.to_string())?;
    } else if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(enabled)
}

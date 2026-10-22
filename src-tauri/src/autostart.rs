//! Login item: HKCU Run key on Windows, a LaunchAgent plist on macOS.
use crate::error::CommandError;
#[cfg(any(windows, target_os = "macos"))]
use crate::error::IoOp;
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
pub fn set_enabled(enabled: bool) -> Result<bool, CommandError> {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        let run = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                KEY_SET_VALUE,
            )
            .map_err(|e| CommandError::io(IoOp::Open, None, &e))?;
        if enabled {
            let exe =
                std::env::current_exe().map_err(|e| CommandError::io(IoOp::Other, None, &e))?;
            run.set_value(
                "QuickerText",
                &format!("\"{}\" --background", exe.display()),
            )
            .map_err(|e| CommandError::io(IoOp::Write, None, &e))?
        } else {
            match run.delete_value("QuickerText") {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(CommandError::io(IoOp::Remove, None, &e)),
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
        Err(CommandError::Unsupported {
            capability: "autostart".into(),
        })
    }
}
#[cfg(target_os = "macos")]
fn mac_login_path() -> Result<PathBuf, CommandError> {
    let user_home = std::env::var_os("HOME")
        .ok_or_else(|| CommandError::internal("macOS home directory is unavailable"))?;
    Ok(PathBuf::from(user_home).join("Library/LaunchAgents/com.quickertext.desktop.plist"))
}
#[cfg(target_os = "macos")]
fn set_mac_autostart(enabled: bool) -> Result<bool, CommandError> {
    let path = mac_login_path()?;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| CommandError::io(IoOp::Other, None, &e))?;
        let escaped = exe
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let content=format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>com.quickertext.desktop</string><key>ProgramArguments</key><array><string>{escaped}</string><string>--background</string></array><key>RunAtLoad</key><true/></dict></plist>");
        let parent = path
            .parent()
            .ok_or_else(|| CommandError::internal("Login item parent unavailable"))?;
        fs::create_dir_all(parent)
            .map_err(|e| CommandError::io(IoOp::CreateDir, Some(parent), &e))?;
        fs::write(&path, content).map_err(|e| CommandError::io(IoOp::Write, Some(&path), &e))?;
    } else if path.exists() {
        fs::remove_file(&path).map_err(|e| CommandError::io(IoOp::Remove, Some(&path), &e))?;
    }
    Ok(enabled)
}

//! Login item: HKCU Run key on Windows, a LaunchAgent plist on macOS.
use crate::error::CommandError;
#[cfg(any(windows, target_os = "macos"))]
use crate::error::IoOp;
use crate::faults;
use quicker_platform::fault::FaultPoint;
#[cfg(target_os = "macos")]
use std::{fs, path::PathBuf};
use std::{io, path::Path};

#[cfg(windows)]
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "QuickerText";

/// The login item as the OS stores it: the Run value on Windows, the plist
/// content on macOS. Behind a trait so that tests never touch the real one.
pub trait LoginItem {
    /// `None` when autostart is off.
    fn entry(&self) -> io::Result<Option<String>>;
    fn set_entry(&self, entry: &str) -> io::Result<()>;
}

pub struct SystemLoginItem;
impl LoginItem for SystemLoginItem {
    fn entry(&self) -> io::Result<Option<String>> {
        #[cfg(windows)]
        {
            use winreg::{enums::*, RegKey};
            let run = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(RUN_KEY) {
                Ok(run) => run,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            match run.get_value::<String, _>(RUN_VALUE) {
                Ok(value) => Ok(Some(value)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }
        #[cfg(target_os = "macos")]
        {
            let path = mac_login_path().map_err(io::Error::other)?;
            match fs::read_to_string(path) {
                Ok(content) => Ok(Some(content)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            Ok(None)
        }
    }
    fn set_entry(&self, entry: &str) -> io::Result<()> {
        #[cfg(windows)]
        {
            use winreg::{enums::*, RegKey};
            RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?
                .set_value(RUN_VALUE, &entry)
        }
        #[cfg(target_os = "macos")]
        {
            fs::write(mac_login_path().map_err(io::Error::other)?, entry)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = entry;
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

/// The login item content that starts `exe` in the background.
fn launch_entry(exe: &Path) -> String {
    if cfg!(target_os = "macos") {
        let escaped = exe
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>com.quickertext.desktop</string><key>ProgramArguments</key><array><string>{escaped}</string><string>--background</string></array><key>RunAtLoad</key><true/></dict></plist>")
    } else {
        format!("\"{}\" --background", exe.display())
    }
}

/// Startup check (B-04): when autostart is on but points somewhere other
/// than `exe` (the exe was moved or upgraded), rewrite it. Returns whether
/// it was repaired. Autostart that is off stays off.
pub fn repair(item: &dyn LoginItem, exe: &Path) -> io::Result<bool> {
    let Some(mut current) = item.entry()? else {
        return Ok(false);
    };
    if faults::take(FaultPoint::AutostartStale).is_some() {
        current = launch_entry(Path::new("stale-quicker-text.exe"));
    }
    let expected = launch_entry(exe);
    let matches = if cfg!(windows) {
        current.trim().eq_ignore_ascii_case(&expected)
    } else {
        current == expected
    };
    if matches {
        return Ok(false);
    }
    item.set_entry(&expected)?;
    tracing::info!(
        event = "autostart.repair",
        "autostart entry now points at this executable"
    );
    Ok(true)
}

pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        use winreg::{enums::*, RegKey};
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(RUN_KEY)
            .and_then(|r| r.get_value::<String, _>(RUN_VALUE))
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
            .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
            .map_err(|e| CommandError::io(IoOp::Open, None, &e))?;
        if enabled {
            let exe =
                std::env::current_exe().map_err(|e| CommandError::io(IoOp::Other, None, &e))?;
            run.set_value(RUN_VALUE, &launch_entry(&exe))
                .map_err(|e| CommandError::io(IoOp::Write, None, &e))?
        } else {
            match run.delete_value(RUN_VALUE) {
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
        let content = launch_entry(&exe);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// In-memory login item; the real one is never touched.
    #[derive(Default)]
    struct FakeLoginItem {
        entry: RefCell<Option<String>>,
        writes: RefCell<u32>,
    }
    impl FakeLoginItem {
        fn with(entry: String) -> Self {
            Self {
                entry: RefCell::new(Some(entry)),
                writes: RefCell::new(0),
            }
        }
    }
    impl LoginItem for FakeLoginItem {
        fn entry(&self) -> io::Result<Option<String>> {
            Ok(self.entry.borrow().clone())
        }
        fn set_entry(&self, entry: &str) -> io::Result<()> {
            *self.entry.borrow_mut() = Some(entry.into());
            *self.writes.borrow_mut() += 1;
            Ok(())
        }
    }

    #[test]
    fn repair_rewrites_only_a_stale_entry() {
        let exe = Path::new("C:/Apps/Quicker Text/quicker-text.exe");
        let off = FakeLoginItem::default();
        assert!(!repair(&off, exe).unwrap());
        assert_eq!(*off.entry.borrow(), None);

        let current = FakeLoginItem::with(launch_entry(exe));
        assert!(!repair(&current, exe).unwrap());
        assert_eq!(*current.writes.borrow(), 0);

        let moved = FakeLoginItem::with(launch_entry(Path::new("D:/old/quicker-text.exe")));
        assert!(repair(&moved, exe).unwrap());
        assert_eq!(*moved.entry.borrow(), Some(launch_entry(exe)));
        assert_eq!(*moved.writes.borrow(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn repair_ignores_case_on_windows() {
        let exe = Path::new("C:/Apps/quicker-text.exe");
        let item = FakeLoginItem::with(launch_entry(exe).to_uppercase());
        assert!(!repair(&item, exe).unwrap());
        assert_eq!(*item.writes.borrow(), 0);
    }

    /// `autostart_stale` fault: the entry reads as stale and is repaired.
    #[cfg(feature = "fault-injection")]
    #[test]
    fn autostart_stale_fault_is_repaired() {
        let _scope = crate::faults::scope::FaultScope::new();
        let exe = Path::new("C:/Apps/quicker-text.exe");
        let item = FakeLoginItem::with(launch_entry(exe));
        quicker_platform::fault::debug_inject_fault(FaultPoint::AutostartStale, 1);
        assert!(repair(&item, exe).unwrap());
        assert_eq!(*item.entry.borrow(), Some(launch_entry(exe)));
        assert_eq!(*item.writes.borrow(), 1);
        // One-shot: the next start finds nothing to repair.
        assert!(!repair(&item, exe).unwrap());
    }
}

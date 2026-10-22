//! Crash-safe writes: temp file + fsync + atomic replace.
use serde::Serialize;
use std::{fs, path::Path};

pub fn atomic_json<T: Serialize + ?Sized>(path: &Path, config: &T) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
    use std::io::Write;
    let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(&bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            if windows_replace(from.as_ptr(), to.as_ptr()) == 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
    }
    #[cfg(not(windows))]
    fs::rename(tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "MoveFileExW"]
    fn move_file_ex(from: *const u16, to: *const u16, flags: u32) -> i32;
}
#[cfg(windows)]
unsafe fn windows_replace(from: *const u16, to: *const u16) -> i32 {
    move_file_ex(from, to, 1 | 8)
}

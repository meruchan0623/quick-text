//! Crash-safe writes: temp file + fsync + atomic replace.
use crate::error::{CommandError, IoOp};
use quicker_platform::fault::FaultPoint;
use serde::Serialize;
use std::{fs, path::Path};

pub fn atomic_json<T: Serialize + ?Sized>(path: &Path, config: &T) -> Result<(), CommandError> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(config).map_err(CommandError::internal)?;
    let write = |e: std::io::Error| CommandError::io(IoOp::Write, Some(path), &e);
    use std::io::Write;
    let mut f = fs::File::create(&tmp).map_err(write)?;
    f.write_all(&bytes).map_err(write)?;
    f.sync_all().map_err(write)?;
    drop(f);
    // `store_write_fail` fault: the replace step fails, as with a full disk
    // or a file held open without sharing; the target stays as it was.
    if crate::faults::take(FaultPoint::StoreWriteFail).is_some() {
        let _ = fs::remove_file(&tmp);
        let e = std::io::Error::other("injected store_write_fail");
        return Err(CommandError::io(IoOp::Rename, Some(path), &e));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            if windows_replace(from.as_ptr(), to.as_ptr()) == 0 {
                let e = std::io::Error::last_os_error();
                return Err(CommandError::io(IoOp::Rename, Some(path), &e));
            }
        }
    }
    #[cfg(not(windows))]
    fs::rename(tmp, path).map_err(|e| CommandError::io(IoOp::Rename, Some(path), &e))?;
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

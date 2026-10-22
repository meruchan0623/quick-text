//! Crash-safe writes: temp file + fsync + atomic replace.
use crate::error::{CommandError, IoOp};
use serde::Serialize;
use std::{fs, io, path::Path};

pub fn atomic_json<T: Serialize + ?Sized>(path: &Path, config: &T) -> Result<(), CommandError> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(config).map_err(CommandError::internal)?;
    let write = |e: std::io::Error| CommandError::io(IoOp::Write, Some(path), &e);
    use std::io::Write;
    let mut f = fs::File::create(&tmp).map_err(write)?;
    f.write_all(&bytes).map_err(write)?;
    f.sync_all().map_err(write)?;
    drop(f);
    replace_file(&tmp, path).map_err(|e| CommandError::io(IoOp::Rename, Some(path), &e))
}

/// Moves `from` over `to` in one step, replacing `to` if it exists:
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` on
/// Windows, `rename` elsewhere. Readers see either the old or the new file.
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            if windows_replace(from.as_ptr(), to.as_ptr()) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    fs::rename(from, to)
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

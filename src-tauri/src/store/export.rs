//! `export_config`: writes the draft as a package to a path the user chose.
//! The target is checked against the safe mode and the data directory with
//! the store lock held, so the checks and the write see the same state.
//!
//! The store lock does not stop other processes from changing the file
//! system, so the package is not written through the checked pathname. It is
//! staged in a new temp file next to the target, the location of that open
//! file is checked again, and the temp file then replaces the target by
//! rename (see [`stage_and_replace`]).
use super::{atomic::replace_file, metadata::metadata_for_config, Store};
use crate::error::{file_label, CommandError, IoOp, SafeModeReason};
use quicker_domain::*;
use std::{
    ffi::OsString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

impl Store {
    /// Exports the draft to `target`.
    ///
    /// In safe mode only SafeMode(CrashLoop) exports: its configuration was
    /// really loaded and the user may want a backup first. In Locked,
    /// NewerSchema and NoValidConfig the draft is not the user's data, and
    /// exporting it could replace a good backup.
    ///
    /// A target inside the data directory is refused in every mode, so an
    /// export never replaces `active.json`, a revision or another store file
    /// behind the atomic writes and the revision logic.
    pub fn export_draft(&self, target: &Path) -> Result<(), CommandError> {
        self.export_draft_with(target, &mut ())
    }

    /// [`Store::export_draft`] with the staging steps behind `hooks`.
    pub(crate) fn export_draft_with(
        &self,
        target: &Path,
        hooks: &mut impl StageHooks,
    ) -> Result<(), CommandError> {
        match self.safe_mode {
            None | Some(SafeModeReason::CrashLoop) => {}
            Some(reason) => return Err(CommandError::SafeMode { reason }),
        }
        // Serialize before touching the file system, so no work sits between
        // the checks below and the write. Originals of unreadable rules go
        // along with them (F4-2).
        let unresolved = metadata_for_config(&self.draft, &self.unresolved);
        let package = export_package_with_unresolved(&self.draft, unresolved)
            .map_err(CommandError::validation)?;
        let data_dir = comparable(
            &fs::canonicalize(&self.dir)
                .or_else(|_| std::path::absolute(&self.dir))
                .unwrap_or_else(|_| self.dir.clone()),
        );
        // Refuse before creating anything, so a target inside the data
        // directory never gets even a temp file there.
        let resolved =
            resolve_target(target).map_err(|e| CommandError::io(IoOp::Write, Some(target), &e))?;
        if comparable(&resolved).starts_with(&data_dir) {
            return Err(refused(target, "target"));
        }
        stage_and_replace(target, &data_dir, package.as_bytes(), hooks)
    }
}

/// The steps of [`stage_and_replace`] that tests replace: production passes
/// `()`, which keeps every default.
pub(crate) trait StageHooks {
    /// Where the open temp file really is.
    fn final_path(&mut self, file: &fs::File, temp: &Path) -> io::Result<PathBuf> {
        final_path(file, temp)
    }
    /// Runs after the temp file passed the containment check, before the
    /// package is written.
    fn after_check(&mut self, _temp: &Path) {}
    /// Writes the package into the temp file.
    fn write(&mut self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }
}
impl StageHooks for () {}

/// Writes `bytes` to `target` without opening `target` itself:
///
/// 1. A temp file with a unique name is created next to the target with
///    `create_new`, so the call fails on any existing entry, links included.
/// 2. The location of the open handle is read back from the OS
///    (`GetFinalPathNameByHandleW` on Windows) and refused if it is inside
///    the data directory. This catches a parent directory or junction
///    redirected into the data directory after the pathname check.
/// 3. The package is written and synced through that handle.
/// 4. The temp file replaces the target by rename ([`replace_file`], the
///    replace step shared with the store files and the diagnostics bundle).
///    A rename replaces the directory entry at the destination and does not
///    follow it: a symlink or hard link that was put there in the meantime,
///    even one pointing at `active.json`, is itself replaced and the file it
///    points to is not written. On Windows `replace_file` is
///    `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`,
///    which acts on the destination name only and returns once the move is
///    on disk.
///
/// On any failure the temp file is removed and the target is left as it was,
/// so a failed export never truncates the user's existing file.
///
/// Accepted residual risk: if the parent directory is redirected into the
/// data directory between step 2 and the rename, the rename lands there.
/// Only a process of the same user can do that, and such a process can
/// already write the store files directly; closing it would need a rename
/// relative to a pinned directory handle (`NtSetInformationFile` with a root
/// directory), which is not worth its weight for an export.
fn stage_and_replace(
    target: &Path,
    data_dir: &Path,
    bytes: &[u8],
    hooks: &mut impl StageHooks,
) -> Result<(), CommandError> {
    let write_error = |e: io::Error| CommandError::io(IoOp::Write, Some(target), &e);
    let name = target
        .file_name()
        .ok_or_else(|| write_error(io::ErrorKind::InvalidInput.into()))?;
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // The target's name keeps a leftover recognisable; it is cut short so
    // the temp name stays within the file name length limit.
    let label: String = stored_name(name)
        .to_string_lossy()
        .chars()
        .take(32)
        .collect();
    let temp = parent.join(format!(
        ".{label}.qt-export-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(write_error)?;
    // `check_and_fill` synced the temp file and closed its handle on return,
    // as `replace_file` requires.
    let result = check_and_fill(file, &temp, target, data_dir, bytes, hooks).and_then(|()| {
        replace_file(&temp, target).map_err(|e| CommandError::io(IoOp::Rename, Some(target), &e))
    });
    if result.is_err() {
        if let Err(e) = fs::remove_file(&temp) {
            tracing::warn!(
                event = "export.temp_cleanup_failed",
                file = ?file_label(&temp),
                error = %e,
                "could not remove the export temp file"
            );
        }
    }
    result
}

/// Steps 2 and 3 of [`stage_and_replace`]; the handle is closed on return.
fn check_and_fill(
    mut file: fs::File,
    temp: &Path,
    target: &Path,
    data_dir: &Path,
    bytes: &[u8],
    hooks: &mut impl StageHooks,
) -> Result<(), CommandError> {
    let write_error = |e: io::Error| CommandError::io(IoOp::Write, Some(target), &e);
    let location = hooks.final_path(&file, temp).map_err(write_error)?;
    if comparable(&location).starts_with(data_dir) {
        return Err(refused(target, "temp_file"));
    }
    hooks.after_check(temp);
    hooks.write(&mut file, bytes).map_err(write_error)?;
    file.sync_all().map_err(write_error)
}

/// The refusal of a target inside the data directory; `stage` tells which
/// check caught it.
fn refused(target: &Path, stage: &'static str) -> CommandError {
    tracing::warn!(
        event = "export.refused_data_dir",
        file = ?file_label(target),
        stage,
        "export target is inside the data directory"
    );
    CommandError::Io {
        op: IoOp::Write,
        path: file_label(target),
        os_code: None,
    }
}

/// The location of the open `file` as the OS reports it, links and 8.3
/// names resolved.
#[cfg(windows)]
pub(super) fn final_path(file: &fs::File, _temp: &Path) -> io::Result<PathBuf> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            file: *mut std::ffi::c_void,
            path: *mut u16,
            len: u32,
            flags: u32,
        ) -> u32;
    }
    let mut buf = vec![0u16; 512];
    loop {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: `file` keeps the handle open for the call and `buf` holds
        // `len` u16s. Flags 0: normalized name with a drive letter.
        let n =
            unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), buf.as_mut_ptr(), len, 0) };
        let n = n as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        // Too small: `n` is the size needed, terminating NUL included.
        buf.resize(n, 0);
    }
}

/// The location of the temp file, its parent directories resolved; the
/// file itself was just created and is not a link.
#[cfg(not(windows))]
pub(super) fn final_path(_file: &fs::File, temp: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(temp)
}

/// The location a write to `target` reaches, resolved as the OS resolves it:
/// an existing target through links and 8.3 names, otherwise its parent
/// directory plus the name the file would get.
fn resolve_target(target: &Path) -> io::Result<PathBuf> {
    if let Ok(existing) = fs::canonicalize(target) {
        return Ok(existing);
    }
    // `..` as the last component has no file name.
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(fs::canonicalize(parent)?.join(stored_name(name)))
}

/// The name Windows creates for `name`: an alternate data stream
/// (`name:stream`) belongs to the file `name`, and trailing dots and spaces
/// are dropped.
fn stored_name(name: &std::ffi::OsStr) -> OsString {
    if !cfg!(windows) {
        return name.to_os_string();
    }
    let text = name.to_string_lossy();
    let file = text.split(':').next().unwrap_or_default();
    OsString::from(file.trim_end_matches(['.', ' ']))
}

/// One spelling per location for the containment test: without the `\\?\`
/// prefix that `canonicalize` adds on Windows, and lower-cased on every
/// platform (a false match only refuses an export).
fn comparable(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    };
    PathBuf::from(text.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparable_drops_the_verbatim_prefix_and_case() {
        if cfg!(windows) {
            assert_eq!(
                comparable(Path::new(r"\\?\C:\Data\Quicker")),
                comparable(Path::new(r"c:\data\quicker"))
            );
            assert_eq!(
                comparable(Path::new(r"\\?\UNC\server\share\x")),
                comparable(Path::new(r"\\server\share\x"))
            );
            assert_eq!(stored_name("active.json. .".as_ref()), "active.json");
            assert_eq!(stored_name("active.json::$DATA".as_ref()), "active.json");
        } else {
            assert_eq!(stored_name("active.json.".as_ref()), "active.json.");
        }
        // Component-wise: a sibling that only shares a prefix is outside.
        assert!(!comparable(Path::new("/data/quicker-backup/x.json"))
            .starts_with(comparable(Path::new("/data/quicker"))));
    }

    /// The handle of a file reports the file's own location.
    #[test]
    fn final_path_reports_where_the_open_file_is() {
        let dir = std::env::temp_dir().join(format!("quicker-export-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        let temp = dir.join("sub").join("..").join("x.tmp");
        let file = fs::File::create(&temp).unwrap();
        let location = final_path(&file, &temp).unwrap();
        drop(file);
        let expected = fs::canonicalize(dir.join("x.tmp")).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(comparable(&location), comparable(&expected));
    }
}

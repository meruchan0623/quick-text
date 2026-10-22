//! `export_config`: writes the draft as a package to a path the user chose.
//! The target is checked against the safe mode and the data directory with
//! the store lock held, so the checks and the write see the same state.
use super::Store;
use crate::error::{file_label, CommandError, IoOp, SafeModeReason};
use quicker_domain::*;
use std::{
    ffi::OsString,
    fs, io,
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
        match self.safe_mode {
            None | Some(SafeModeReason::CrashLoop) => {}
            Some(reason) => return Err(CommandError::SafeMode { reason }),
        }
        let resolved =
            resolve_target(target).map_err(|e| CommandError::io(IoOp::Write, Some(target), &e))?;
        let data_dir = fs::canonicalize(&self.dir)
            .or_else(|_| std::path::absolute(&self.dir))
            .unwrap_or_else(|_| self.dir.clone());
        if comparable(&resolved).starts_with(comparable(&data_dir)) {
            tracing::warn!(
                event = "export.refused_data_dir",
                file = ?file_label(target),
                "export target is inside the data directory"
            );
            return Err(CommandError::Io {
                op: IoOp::Write,
                path: file_label(target),
                os_code: None,
            });
        }
        let unresolved = self
            .unresolved
            .iter()
            .filter(|x| self.draft.rules.iter().any(|r| r.id == x.rule_id))
            .cloned()
            .collect();
        let package = export_package_with_unresolved(&self.draft, unresolved)
            .map_err(CommandError::validation)?;
        fs::write(target, package).map_err(|e| CommandError::io(IoOp::Write, Some(target), &e))
    }
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
}

//! `backup-pre-0.4/` (plan §9, B-43, S-04 P0): before 0.4 writes a file
//! that 0.3.1 reads for the first time, the files 0.3.1 needs are copied
//! byte for byte into `backup-pre-0.4/` in the data directory, so going back
//! to 0.3.1 can start from exactly what it left.
//!
//! - Gate: every write of such a file goes through [`Store::write_data`]
//!   (draft, discard, publish and rollback, the metadata snapshot at load,
//!   the last-known-good snapshot). `store-meta.json`, `prefs.json`, logs,
//!   quarantine renames and exports are not such files and do not trigger it.
//! - Once: `store-meta.json` records the outcome in `backup_pre_0_4`. While
//!   it is absent the gate looks at the directory: with no file 0.3.1 reads
//!   there is nothing to back up (a new data directory) and only the mark is
//!   set; a complete `backup-pre-0.4/` that already exists is never touched.
//! - Atomic: the copies are written and flushed in
//!   `backup-pre-0.4.tmp-<UTC time>/` and that directory is renamed to
//!   `backup-pre-0.4/` as the last step. A temp directory left by an
//!   interrupted attempt is removed and the backup made again.
//! - Failure: nothing is written; the write is refused with an `io` error
//!   whose path is `backup-pre-0.4`, a `backup_failed` problem is listed and
//!   the next write tries again. The engine keeps running on the active
//!   configuration.
//!
//! Known limit: going back to 0.3.1, changing data there and upgrading
//! again makes no second backup (the mark is already set).
use super::{atomic::atomic_json, prefs::utc_stamp, Store};
use crate::{
    error::{file_label, CommandError, IoOp},
    startup::ProblemKind,
};
use serde::Serialize;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

pub const BACKUP_DIR: &str = "backup-pre-0.4";
const TEMP_PREFIX: &str = "backup-pre-0.4.tmp-";
/// Files in the data directory that 0.3.1 reads; any of them present means
/// the directory holds data from before 0.4 (as do the revision files, see
/// [`is_revision_file`]).
const LEGACY_FILES: [&str; 5] = [
    "active.json",
    "draft.json",
    "previous.json",
    "migration-unresolved.json",
    "active-migration-unresolved.json",
];
/// Copied as well when present; on their own they do not call for a backup.
const EXTRA_FILES: [&str; 2] = ["prefs.json", "revisions/index.json"];

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Created {
        files: usize,
    },
    /// `backup-pre-0.4/` was already there; it is left as it is.
    Existing,
    /// No file 0.3.1 reads: nothing to back up.
    NotNeeded,
}

struct Failure {
    op: IoOp,
    /// File name (`file_label`) of what failed; never a full path.
    file: Option<String>,
    error: io::Error,
    /// Files copied before the failure (the copies are removed again).
    copied: usize,
}

impl Failure {
    fn new(op: IoOp, path: &Path, error: io::Error, copied: usize) -> Self {
        Self {
            op,
            file: file_label(path),
            error,
            copied,
        }
    }
}

/// `<n>.json` or `<n>.metadata.json` in `revisions/`.
fn is_revision_file(name: &str) -> bool {
    let stem = name
        .strip_suffix(".metadata.json")
        .or_else(|| name.strip_suffix(".json"));
    stem.is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `dir` holds a file that 0.3.1 reads. What cannot be checked
/// counts as present: the backup is then attempted, and refuses the write if
/// it cannot read the file either.
fn has_legacy_data(dir: &Path) -> bool {
    if LEGACY_FILES
        .iter()
        .any(|name| !matches!(dir.join(name).try_exists(), Ok(false)))
    {
        return true;
    }
    match fs::read_dir(dir.join("revisions")) {
        Ok(mut entries) => entries.any(|entry| match entry {
            Ok(entry) => entry.file_name().to_str().is_some_and(is_revision_file),
            Err(_) => true,
        }),
        Err(e) => e.kind() != io::ErrorKind::NotFound,
    }
}

/// The files to copy, relative to the data directory: the legacy files, the
/// snapshots and metadata of `revisions`, and the extra files.
fn backup_files(revisions: &[u64]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = LEGACY_FILES.iter().map(PathBuf::from).collect();
    for revision in revisions {
        for name in [
            format!("{revision}.json"),
            format!("{revision}.metadata.json"),
        ] {
            let path = Path::new("revisions").join(name);
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    files.extend(EXTRA_FILES.iter().map(|name| {
        name.split('/')
            .fold(PathBuf::new(), |path, part| path.join(part))
    }));
    files
}

/// Removes `backup-pre-0.4.tmp-*` directories an interrupted attempt left.
/// They are incomplete copies; failing to remove one only leaves it there.
fn remove_stale_temps(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TEMP_PREFIX) {
            continue;
        }
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => tracing::info!(
                event = "store.backup_pre_0_4",
                result = "stale_temp_removed",
                file = name,
                "temp directory of an interrupted backup removed"
            ),
            Err(e) => tracing::warn!(
                event = "store.backup_pre_0_4",
                result = "stale_temp_kept",
                file = name,
                error_kind = ?e.kind(),
                os_code = e.raw_os_error(),
                "temp directory of an interrupted backup not removed"
            ),
        }
    }
}

/// A new, empty temp directory in `dir`.
fn create_temp(dir: &Path) -> io::Result<PathBuf> {
    let stamp = utc_stamp(SystemTime::now());
    let mut attempt = 0;
    loop {
        let name = match attempt {
            0 => format!("{TEMP_PREFIX}{stamp}"),
            n => format!("{TEMP_PREFIX}{stamp}-{n}"),
        };
        let temp = dir.join(name);
        match fs::create_dir(&temp) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
            result => return result.map(|()| temp),
        }
    }
}

/// Byte-for-byte copy, flushed before it returns.
fn copy_file(bytes: &[u8], to: &Path) -> io::Result<()> {
    let mut file = fs::File::create(to)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Copies `files` from `dir` into `temp`; files that do not exist are
/// skipped. Returns how many were copied.
fn copy_all(dir: &Path, temp: &Path, files: &[PathBuf]) -> Result<usize, Failure> {
    let mut copied = 0;
    for relative in files {
        let from = dir.join(relative);
        let bytes = match fs::read(&from) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Failure::new(IoOp::Read, &from, e, copied)),
        };
        let to = temp.join(relative);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Failure::new(IoOp::CreateDir, parent, e, copied))?;
        }
        copy_file(&bytes, &to).map_err(|e| Failure::new(IoOp::Write, &from, e, copied))?;
        copied += 1;
    }
    Ok(copied)
}

/// Makes `backup-pre-0.4/` in `dir` unless it exists or is not needed.
fn run(dir: &Path, revisions: &[u64]) -> Result<Outcome, Failure> {
    remove_stale_temps(dir);
    let target = dir.join(BACKUP_DIR);
    match fs::metadata(&target) {
        Ok(metadata) if metadata.is_dir() => return Ok(Outcome::Existing),
        // A file of that name: the backup cannot be made, and is not there.
        Ok(_) => {
            let e = io::Error::from(io::ErrorKind::AlreadyExists);
            return Err(Failure::new(IoOp::CreateDir, &target, e, 0));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Failure::new(IoOp::Read, &target, e, 0)),
    }
    if !has_legacy_data(dir) {
        return Ok(Outcome::NotNeeded);
    }
    let temp = create_temp(dir).map_err(|e| Failure::new(IoOp::CreateDir, &target, e, 0))?;
    let copied = copy_all(dir, &temp, &backup_files(revisions)).and_then(|copied| {
        fs::rename(&temp, &target)
            .map(|()| copied)
            .map_err(|e| Failure::new(IoOp::Rename, &target, e, copied))
    });
    if copied.is_err() {
        // Best effort; whatever stays is removed by the next attempt.
        let _ = fs::remove_dir_all(&temp);
    }
    copied.map(|files| Outcome::Created { files })
}

impl Store {
    /// The one way to write a file that 0.3.1 reads: the backup gate, then
    /// the crash-safe write.
    pub(super) fn write_data<T: Serialize + ?Sized>(
        &self,
        path: &Path,
        value: &T,
    ) -> Result<(), CommandError> {
        self.backup_pre_0_4()?;
        atomic_json(path, value)
    }
    /// Makes `backup-pre-0.4/` if `store-meta.json` does not say it has
    /// been dealt with. An error means nothing may be written yet.
    pub(super) fn backup_pre_0_4(&self) -> Result<(), CommandError> {
        if self.meta.read(|meta| meta.backup_pre_0_4.is_some()) {
            return Ok(());
        }
        let outcome = match run(&self.dir, &self.backup_revisions()) {
            Ok(outcome) => outcome,
            Err(failure) => {
                tracing::warn!(
                    event = "store.backup_pre_0_4",
                    result = "failed",
                    files = failure.copied,
                    op = ?failure.op,
                    file = failure.file.as_deref().unwrap_or_default(),
                    error_kind = ?failure.error.kind(),
                    os_code = failure.error.raw_os_error(),
                    "backup before the first 0.4 write failed; nothing written"
                );
                return Err(CommandError::io(
                    failure.op,
                    Some(Path::new(BACKUP_DIR)),
                    &failure.error,
                ));
            }
        };
        let (result, files) = match outcome {
            Outcome::Created { files } => ("created", files),
            Outcome::Existing => ("existing", 0),
            Outcome::NotNeeded => ("not_needed", 0),
        };
        tracing::info!(
            event = "store.backup_pre_0_4",
            result,
            files,
            "backup before the first 0.4 write settled"
        );
        let backed_up = outcome != Outcome::NotNeeded;
        if let Err(e) = self
            .meta
            .update(|meta| meta.backup_pre_0_4 = Some(backed_up))
        {
            tracing::warn!(event = "store.meta_write_failed", error = %e, "store-meta.json not written");
        }
        Ok(())
    }
    /// The gate at the start of a write command, which also lists or clears
    /// the `backup_failed` problem.
    pub(super) fn backup_before_write(&mut self) -> Result<(), CommandError> {
        let result = self.backup_pre_0_4();
        self.note_backup(result.is_ok());
        result
    }
    /// Lists `backup_failed` once while the backup fails. Once it succeeds
    /// the problem is dropped and the metadata snapshot that the load could
    /// not write without the backup is written, before the write that
    /// called the gate replaces anything.
    pub(super) fn note_backup(&mut self, ok: bool) {
        let failed = |kind: &ProblemKind| matches!(kind, ProblemKind::BackupFailed { .. });
        let listed = self.startup_problems.iter().any(|p| failed(&p.kind));
        if ok && listed {
            self.startup_problems.retain(|p| !failed(&p.kind));
            if self.safe_mode.is_none() {
                self.snapshot_active_metadata();
            }
        } else if !ok && !listed {
            self.add_problem(ProblemKind::BackupFailed {
                backup: BACKUP_DIR.into(),
            });
        }
    }
    /// The current and previous revision: the active one, the one before it
    /// (what 0.3.1 rolls back to) and the recorded predecessor.
    fn backup_revisions(&self) -> Vec<u64> {
        let active = self.active.revision;
        let mut revisions = vec![active];
        revisions.extend(active.checked_sub(1));
        revisions.extend(self.previous.as_ref().map(|config| config.revision));
        revisions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_file_names() {
        for name in ["1.json", "12.metadata.json", "0.json"] {
            assert!(is_revision_file(name), "{name}");
        }
        for name in [
            "index.json",
            ".json",
            ".metadata.json",
            "1.json.tmp",
            "a1.json",
        ] {
            assert!(!is_revision_file(name), "{name}");
        }
    }

    #[test]
    fn backup_files_cover_current_and_previous_revisions() {
        let files = backup_files(&[3, 2, 2]);
        let names: Vec<String> = files
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            names,
            [
                "active.json",
                "draft.json",
                "previous.json",
                "migration-unresolved.json",
                "active-migration-unresolved.json",
                "revisions/3.json",
                "revisions/3.metadata.json",
                "revisions/2.json",
                "revisions/2.metadata.json",
                "prefs.json",
                "revisions/index.json",
            ]
        );
    }
}

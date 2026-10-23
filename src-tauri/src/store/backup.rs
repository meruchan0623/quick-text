//! `backup-pre-0.4/` (plan §9, B-43, S-04 P0): before 0.4 changes a file
//! that 0.3.1 reads for the first time, the files 0.3.1 needs are copied
//! byte for byte into `backup-pre-0.4/` in the data directory, so going back
//! to 0.3.1 can start from exactly what it left.
//!
//! - Gate: every write of such a file goes through [`Store::write_data`]
//!   (draft, discard, publish and rollback, the metadata snapshot at load,
//!   the last-known-good snapshot). Renaming a corrupt one aside waits for
//!   the backup as well (review I-B1): the load leaves it where it is and
//!   the store renames it once the backup holds it, at the next write
//!   again while the rename fails ([`Store::quarantine_deferred`]).
//!   `store-meta.json` and `prefs.json` do not trigger the gate because
//!   0.3.1 never reads them: both files are new in 0.4. Logs and exports do
//!   not either.
//! - Contents: the files 0.3.1 reads, the snapshot and metadata of the
//!   active revision, of the one before it and of the recorded predecessor,
//!   the corrupt files the load left in place and the quarantined ones, and
//!   for each revision snapshot among them, corrupt or not, its
//!   `<n>.metadata.json` (review R7-1).
//! - Once: `store-meta.json` records the outcome in `backup_pre_0_4` and the
//!   directory in `backup_pre_0_4_dir`. While the mark is absent the gate
//!   looks at the directory: a complete backup found there is taken as it
//!   is; with no file 0.3.1 reads and no quarantined file (`*.corrupt-*`,
//!   review I-B2) it is a new data directory, nothing is backed up and the
//!   mark is `false`.
//! - Complete (review I-B3): only a backup with a valid `manifest.json` is
//!   complete. The manifest lists every copied file with its path relative
//!   to the backup, its length and SHA-256, and is written last. A backup
//!   found in the directory is checked against it before it is taken; one
//!   that fails the check is never changed or removed, and the new backup
//!   goes to `backup-pre-0.4-<UTC time>/` beside it.
//! - Atomic: the copies and the manifest are written and flushed in
//!   `backup-pre-0.4.tmp-<UTC time>/`, and that directory is renamed to its
//!   final name as the last step. (The directory entry itself is not
//!   flushed: std offers no directory fsync on Windows.) A temp directory
//!   left by an interrupted attempt is removed and the backup made again.
//! - Failure: only a new backup that cannot be made refuses writes. Nothing
//!   is written or renamed; the write is refused with an `io` error whose
//!   path is `backup-pre-0.4`, a `backup_failed` problem is listed and the
//!   next write tries again. The engine keeps running on the active
//!   configuration.
//!
//! Known limits: going back to 0.3.1, changing data there and upgrading
//! again makes no second backup (the mark is already set). Once the mark is
//! `true` the backup is not checked again: a recorded directory that is
//! gone is only logged at load (`store.backup_missing`, review R7-3).
use super::{
    atomic::atomic_json, fallback::quarantine_target, meta::Fingerprint, prefs::utc_stamp, Store,
};
use crate::{
    error::{file_label, CommandError, IoOp},
    startup::{ProblemKind, StartupProblem},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

pub const BACKUP_DIR: &str = "backup-pre-0.4";
/// `backup-pre-0.4-<UTC time>`: where the backup goes when something that
/// is not a complete backup is in the way at `backup-pre-0.4`.
const DATED_PREFIX: &str = "backup-pre-0.4-";
const TEMP_PREFIX: &str = "backup-pre-0.4.tmp-";
/// In the backup directory, written last (I-B3).
const MANIFEST: &str = "manifest.json";
const MANIFEST_VERSION: u32 = 1;
/// In the name of every quarantined file (`<stem>.corrupt-<UTC time>.json`).
const QUARANTINE_MARK: &str = ".corrupt-";
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
/// `revisions/index.json` is 0.3.1's. `prefs.json` is not: 0.3.1 never
/// reads it, it is new in 0.4 and never goes through the gate. It is only
/// copied along with the rest when 0.4 wrote it before the backup was made.
const EXTRA_FILES: [&str; 2] = ["prefs.json", "revisions/index.json"];

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Made in `dir` (a name in the data directory).
    Created { dir: String, files: usize },
    /// A complete backup was already there; it is left as it is.
    Existing { dir: String },
    /// No file 0.3.1 reads and nothing quarantined: nothing to back up.
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

/// `manifest.json` of a backup (I-B3).
#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    files: Vec<ManifestEntry>,
}
#[derive(Serialize, Deserialize)]
struct ManifestEntry {
    /// Relative to the backup directory, `/` between the parts.
    path: String,
    /// `len` and `sha256` of the copy.
    #[serde(flatten)]
    fingerprint: Fingerprint,
}

/// `<n>.json` or `<n>.metadata.json` in `revisions/`.
fn is_revision_file(name: &str) -> bool {
    let stem = name
        .strip_suffix(".metadata.json")
        .or_else(|| name.strip_suffix(".json"));
    stem.is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
}

fn is_quarantined(name: &str) -> bool {
    name.contains(QUARANTINE_MARK)
}

/// Whether `dir` holds a file that 0.3.1 reads, or a quarantined file (I-B2:
/// a directory whose candidates were all moved aside is no new one). What
/// cannot be checked counts as present: the backup is then attempted, and
/// refuses the write if it cannot read the file either.
fn has_legacy_data(dir: &Path) -> bool {
    if LEGACY_FILES
        .iter()
        .any(|name| !matches!(dir.join(name).try_exists(), Ok(false)))
    {
        return true;
    }
    let any = |dir: &Path, matches: &dyn Fn(&str) -> bool| match fs::read_dir(dir) {
        Ok(mut entries) => entries.any(|entry| match entry {
            Ok(entry) => entry.file_name().to_str().is_some_and(matches),
            Err(_) => true,
        }),
        Err(e) => e.kind() != io::ErrorKind::NotFound,
    };
    any(dir, &is_quarantined)
        || any(&dir.join("revisions"), &|name| {
            is_revision_file(name) || is_quarantined(name)
        })
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
    files.extend(EXTRA_FILES.iter().map(|name| relative_path(name)));
    files
}

/// `a/b` as a relative path.
fn relative_path(name: &str) -> PathBuf {
    name.split('/')
        .fold(PathBuf::new(), |path, part| path.join(part))
}

/// The quarantined files beside `store-meta.json` and in `revisions/`,
/// relative to the data directory (I-B2: they are the originals of
/// candidates an earlier launch moved aside).
fn quarantined_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = vec![];
    for sub in ["", "revisions"] {
        let Ok(entries) = fs::read_dir(dir.join(sub)) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
            if let Some(name) = entry.file_name().to_str() {
                if is_file && is_quarantined(name) {
                    found.push(Path::new(sub).join(name));
                }
            }
        }
    }
    found.sort();
    found
}

/// `revisions/<n>.metadata.json` for `revisions/<n>.json` or a quarantined
/// `revisions/<n>.corrupt-*.json` (review R7-1): 0.3.1 reads the metadata
/// beside the snapshot, and it must be in the backup before anything may
/// write that number again.
fn metadata_companion(relative: &Path) -> Option<PathBuf> {
    let name = relative.strip_prefix("revisions").ok()?.to_str()?;
    let stem = name.strip_suffix(".json")?;
    let number = stem
        .split_once(QUARANTINE_MARK)
        .map_or(stem, |(number, _)| number);
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        .then(|| Path::new("revisions").join(format!("{number}.metadata.json")))
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
/// skipped. Returns the manifest entries of the copies.
fn copy_all(dir: &Path, temp: &Path, files: &[PathBuf]) -> Result<Vec<ManifestEntry>, Failure> {
    let mut copied = vec![];
    for relative in files {
        let from = dir.join(relative);
        let bytes = match fs::read(&from) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Failure::new(IoOp::Read, &from, e, copied.len())),
        };
        let to = temp.join(relative);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Failure::new(IoOp::CreateDir, parent, e, copied.len()))?;
        }
        copy_file(&bytes, &to).map_err(|e| Failure::new(IoOp::Write, &from, e, copied.len()))?;
        let parts: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        copied.push(ManifestEntry {
            path: parts.join("/"),
            fingerprint: Fingerprint::of(&bytes),
        });
    }
    Ok(copied)
}

/// Writes and flushes the manifest of the copies in `temp`, the last file of
/// the backup. Returns how many files it lists.
fn write_manifest(temp: &Path, files: Vec<ManifestEntry>) -> Result<usize, Failure> {
    let count = files.len();
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        files,
    };
    let bytes = serde_json::to_vec_pretty(&manifest).expect("manifest serialization");
    let path = temp.join(MANIFEST);
    copy_file(&bytes, &path).map_err(|e| Failure::new(IoOp::Write, &path, e, count))?;
    Ok(count)
}

/// Checks the backup in `backup` against its manifest: every listed file
/// exists with its length and SHA-256. The number of files, or why the
/// backup is incomplete.
fn verify(backup: &Path) -> Result<usize, &'static str> {
    if !fs::metadata(backup).is_ok_and(|metadata| metadata.is_dir()) {
        return Err("not_a_directory");
    }
    let bytes = match fs::read(backup.join(MANIFEST)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err("manifest_missing"),
        Err(_) => return Err("manifest_unreadable"),
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| "manifest_invalid")?;
    if manifest.version != MANIFEST_VERSION {
        return Err("manifest_invalid");
    }
    for entry in &manifest.files {
        let path = manifest_path(&entry.path).ok_or("manifest_invalid")?;
        let bytes = match fs::read(backup.join(path)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err("file_missing"),
            Err(_) => return Err("file_unreadable"),
        };
        if Fingerprint::of(&bytes) != entry.fingerprint {
            return Err("file_changed");
        }
    }
    Ok(manifest.files.len())
}

/// A manifest path as a path inside the backup; `None` for anything that
/// could leave it (`..`, a root, a drive) or is empty.
fn manifest_path(path: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = path.split('/').collect();
    let plain = |part: &&str| {
        !part.is_empty() && *part != "." && *part != ".." && !part.contains(['\\', ':'])
    };
    parts
        .iter()
        .all(plain)
        .then(|| parts.iter().collect::<PathBuf>())
}

/// The backups that may already be in `dir`: `backup-pre-0.4` first, then
/// the dated ones newest first.
fn existing_backups(dir: &Path) -> Vec<String> {
    let mut dated: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.starts_with(DATED_PREFIX))
                .collect()
        })
        .unwrap_or_default();
    dated.sort_unstable_by(|a, b| b.cmp(a));
    let fixed = fs::symlink_metadata(dir.join(BACKUP_DIR)).is_ok();
    fixed
        .then(|| BACKUP_DIR.to_string())
        .into_iter()
        .chain(dated)
        .collect()
}

/// Where a new backup goes: `backup-pre-0.4` when nothing is there, else
/// a dated name that is free.
fn free_name(dir: &Path) -> String {
    let taken = |name: &str| !matches!(fs::symlink_metadata(dir.join(name)), Err(e) if e.kind() == io::ErrorKind::NotFound);
    if !taken(BACKUP_DIR) {
        return BACKUP_DIR.to_string();
    }
    let stamp = utc_stamp(SystemTime::now());
    let mut name = format!("{DATED_PREFIX}{stamp}");
    let mut n = 1;
    while taken(&name) {
        name = format!("{DATED_PREFIX}{stamp}-{n}");
        n += 1;
    }
    name
}

/// Takes a complete backup found in `dir`, or makes one unless there is
/// nothing to back up. `files` are relative to `dir`; the quarantined files
/// are added, and the metadata of every revision snapshot among them, of a
/// corrupt one too (R7-1).
fn run(dir: &Path, files: &[PathBuf]) -> Result<Outcome, Failure> {
    remove_stale_temps(dir);
    for name in existing_backups(dir) {
        match verify(&dir.join(&name)) {
            Ok(_) => return Ok(Outcome::Existing { dir: name }),
            Err(reason) => tracing::warn!(
                event = "store.backup_pre_0_4",
                result = "existing_incomplete",
                dir = %name,
                reason,
                "not a complete backup; left as it is"
            ),
        }
    }
    if !has_legacy_data(dir) {
        return Ok(Outcome::NotNeeded);
    }
    let name = free_name(dir);
    let target = dir.join(&name);
    let mut files = files.to_vec();
    for path in quarantined_files(dir) {
        if !files.contains(&path) {
            files.push(path);
        }
    }
    let companions: Vec<PathBuf> = files
        .iter()
        .filter_map(|path| metadata_companion(path))
        .collect();
    for path in companions {
        if !files.contains(&path) {
            files.push(path);
        }
    }
    let temp = create_temp(dir).map_err(|e| Failure::new(IoOp::CreateDir, &target, e, 0))?;
    let made = copy_all(dir, &temp, &files)
        .and_then(|copied| write_manifest(&temp, copied))
        .and_then(|copied| {
            fs::rename(&temp, &target)
                .map(|()| copied)
                .map_err(|e| Failure::new(IoOp::Rename, &target, e, copied))
        });
    if made.is_err() {
        // Best effort; whatever stays is removed by the next attempt.
        let _ = fs::remove_dir_all(&temp);
    }
    made.map(|files| Outcome::Created { dir: name, files })
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
    /// Makes the pre-0.4 backup if `store-meta.json` does not say it has
    /// been dealt with. An error means nothing may be written yet. The
    /// corrupt files the load left in place are copied too, wherever they
    /// are (I-B1).
    pub(super) fn backup_pre_0_4(&self) -> Result<(), CommandError> {
        if self.meta.read(|meta| meta.backup_pre_0_4.is_some()) {
            return Ok(());
        }
        let mut files = backup_files(&self.backup_revisions());
        for entry in &self.deferred {
            if let Ok(relative) = entry.path.strip_prefix(&self.dir) {
                if !files.iter().any(|file| file == relative) {
                    files.push(relative.to_path_buf());
                }
            }
        }
        let outcome = match run(&self.dir, &files) {
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
        let (result, files, dir) = match outcome {
            Outcome::Created { dir, files } => ("created", files, Some(dir)),
            Outcome::Existing { dir } => ("existing", 0, Some(dir)),
            Outcome::NotNeeded => ("not_needed", 0, None),
        };
        tracing::info!(
            event = "store.backup_pre_0_4",
            result,
            files,
            dir = dir.as_deref().unwrap_or_default(),
            "backup before the first 0.4 write settled"
        );
        if let Err(e) = self.meta.update(|meta| {
            meta.backup_pre_0_4 = Some(dir.is_some());
            meta.backup_pre_0_4_dir = dir;
        }) {
            tracing::warn!(event = "store.meta_write_failed", error = %e, "store-meta.json not written");
        }
        Ok(())
    }
    /// The gate at the start of a write command, which also lists or clears
    /// the `backup_failed` problem and, once the backup has been dealt with,
    /// renames the corrupt files the load left in place (I-B1).
    pub(super) fn backup_before_write(&mut self) -> Result<(), CommandError> {
        let result = self.backup_pre_0_4();
        self.note_backup(result.is_ok());
        result?;
        let mut problems = vec![];
        let result = self.quarantine_deferred(&mut problems);
        for problem in problems {
            self.report_problem(&problem);
            self.startup_problems.push(problem);
        }
        result
    }
    /// Renames the corrupt files the load left in place (I-B1) to
    /// `<stem>.corrupt-<UTC time of the load>.json`, as the load would have,
    /// once `store-meta.json` says the pre-0.4 backup has been dealt with.
    /// Each rename adds a `config_quarantined` problem to `problems`. A file
    /// that changed or went away since the load is left alone and
    /// forgotten (its fingerprint no longer matches). One that cannot be
    /// read or renamed (another program holds it without delete sharing)
    /// must not be overwritten: it stays listed, logged as
    /// `store.quarantine_failed`, and the next call tries again (integration
    /// fix (d)). If it is a file this store writes
    /// ([`Store::replaced_by_writes`]) the error refuses the write that
    /// called the gate; any other only stays listed.
    pub(super) fn quarantine_deferred(
        &mut self,
        problems: &mut Vec<StartupProblem>,
    ) -> Result<(), CommandError> {
        if self.deferred.is_empty() || self.meta.read(|meta| meta.backup_pre_0_4.is_none()) {
            return Ok(());
        }
        let mut refused = None;
        let mut kept = vec![];
        for entry in std::mem::take(&mut self.deferred) {
            let file = file_label(&entry.path).unwrap_or_default();
            let (op, error) = match fs::read(&entry.path) {
                Ok(bytes) if Fingerprint::of(&bytes) == entry.fingerprint => {
                    let target = quarantine_target(&entry.path, entry.found_ms);
                    match fs::rename(&entry.path, &target) {
                        Ok(()) => {
                            let quarantined_as = file_label(&target).unwrap_or_default();
                            tracing::warn!(
                                event = "store.quarantine",
                                file = %file,
                                quarantined_as = %quarantined_as,
                                "quarantined corrupt file"
                            );
                            problems.push(
                                ProblemKind::ConfigQuarantined {
                                    file,
                                    quarantined_as,
                                }
                                .into(),
                            );
                            continue;
                        }
                        Err(e) => (IoOp::Rename, e),
                    }
                }
                Err(e) if e.kind() != io::ErrorKind::NotFound => (IoOp::Read, e),
                _ => {
                    tracing::info!(
                        event = "store.quarantine_stale",
                        file = %file,
                        "corrupt file changed or removed since the load; left as it is"
                    );
                    continue;
                }
            };
            tracing::warn!(
                event = "store.quarantine_failed",
                file = %file,
                error = %error,
                "corrupt file could not be renamed; not overwritten, tried again with the next write"
            );
            if self.replaced_by_writes(&entry.path) {
                refused.get_or_insert_with(|| CommandError::io(op, Some(&entry.path), &error));
            }
            kept.push(entry);
        }
        self.deferred = kept;
        refused.map_or(Ok(()), Err)
    }
    /// Whether a write command of this store may replace `path`; checked
    /// against the targets of `write_data`:
    /// - `draft.json` and `migration-unresolved.json` (save, discard, and
    ///   publish, rollback and the import, which saves then publishes);
    /// - `active.json` and `active-migration-unresolved.json` (publish);
    /// - `revisions/<active>.json`: read, and written when missing, by
    ///   publish (the snapshot of the revision it replaces) and
    ///   [`Store::mark_last_good`];
    /// - `revisions/<active>.metadata.json` (publish, and the metadata
    ///   snapshot of `snapshot_active_metadata`).
    ///
    /// The new revision's `revisions/<new>.metadata.json` is not listed: its
    /// number is above every file in `revisions/`, a corrupt snapshot left in
    /// place included. `store-meta.json` and `prefs.json` are never deferred,
    /// and `previous.json` and older snapshots are never written.
    fn replaced_by_writes(&self, path: &Path) -> bool {
        let revisions = self.dir.join("revisions");
        let active = self.active.revision;
        [
            "active.json",
            "draft.json",
            "migration-unresolved.json",
            "active-migration-unresolved.json",
        ]
        .iter()
        .map(|name| self.dir.join(name))
        .chain([
            revisions.join(format!("{active}.json")),
            revisions.join(format!("{active}.metadata.json")),
        ])
        .any(|written| written == path)
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
    fn metadata_companions_of_snapshots() {
        let companion = |name: &str| {
            metadata_companion(&relative_path(name))
                .map(|path| path.to_string_lossy().replace('\\', "/"))
        };
        for (name, metadata) in [
            ("revisions/1.json", "revisions/1.metadata.json"),
            (
                "revisions/12.corrupt-20260101-000000.json",
                "revisions/12.metadata.json",
            ),
            (
                "revisions/3.corrupt-20260101-000000-1.json",
                "revisions/3.metadata.json",
            ),
        ] {
            assert_eq!(companion(name).as_deref(), Some(metadata), "{name}");
        }
        for name in [
            "1.json",
            "active.json",
            "active.corrupt-20260101-000000.json",
            "revisions/1.metadata.json",
            "revisions/index.json",
            "revisions/.json",
            "revisions/a1.json",
            "revisions/1.json.tmp",
        ] {
            assert_eq!(companion(name), None, "{name}");
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

    /// Review I-B3 (test 7): a backup is complete only while every file its
    /// manifest lists is there with the same length and content. A missing
    /// file or one changed byte makes it incomplete, and so does a missing,
    /// broken or escaping manifest.
    #[test]
    fn manifest_check_finds_missing_and_changed_files() {
        let dir = std::env::temp_dir().join(format!("quicker-backup-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("revisions")).unwrap();
        fs::write(dir.join("active.json"), b"{\"a\":1}\r\n").unwrap();
        fs::write(dir.join("revisions/2.json"), b"{}").unwrap();
        let outcome = run(&dir, &backup_files(&[2])).ok().unwrap();
        assert_eq!(
            outcome,
            Outcome::Created {
                dir: BACKUP_DIR.into(),
                files: 2
            }
        );
        let backup = dir.join(BACKUP_DIR);
        assert_eq!(verify(&backup), Ok(2));
        let manifest: Value =
            serde_json::from_slice(&fs::read(backup.join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(manifest["version"], 1);
        assert_eq!(manifest["files"][1]["path"], "revisions/2.json");
        assert_eq!(manifest["files"][1]["len"], 2);

        // One byte changed, same length.
        let copy = backup.join("active.json");
        fs::write(&copy, b"{\"a\":2}\r\n").unwrap();
        assert_eq!(verify(&backup), Err("file_changed"));
        fs::write(&copy, b"{\"a\":1}\r\n").unwrap();
        assert_eq!(verify(&backup), Ok(2));
        // Cut short.
        fs::write(&copy, b"{\"a\":1}").unwrap();
        assert_eq!(verify(&backup), Err("file_changed"));
        fs::write(&copy, b"{\"a\":1}\r\n").unwrap();
        // A listed file deleted.
        fs::remove_file(backup.join("revisions/2.json")).unwrap();
        assert_eq!(verify(&backup), Err("file_missing"));
        fs::write(backup.join("revisions/2.json"), b"{}").unwrap();
        assert_eq!(verify(&backup), Ok(2));
        // The manifest itself.
        let good = fs::read(backup.join(MANIFEST)).unwrap();
        fs::write(backup.join(MANIFEST), &good[..good.len() - 1]).unwrap();
        assert_eq!(verify(&backup), Err("manifest_invalid"));
        for path in ["../active.json", "/active.json", "C:/x", "a//b", ""] {
            let escaping = serde_json::json!({
                "version": 1,
                "files": [{"path": path, "len": 0, "sha256": ""}]
            });
            fs::write(backup.join(MANIFEST), escaping.to_string()).unwrap();
            assert_eq!(verify(&backup), Err("manifest_invalid"), "{path}");
        }
        fs::remove_file(backup.join(MANIFEST)).unwrap();
        assert_eq!(verify(&backup), Err("manifest_missing"));
        assert_eq!(verify(&dir.join("active.json")), Err("not_a_directory"));
        fs::remove_dir_all(dir).unwrap();
    }

    use serde_json::Value;
}

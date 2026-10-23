//! `load_or_fallback` (B-04, plan §7.2): chooses the configuration to start
//! with, quarantines corrupt files and decides on safe mode. It never fails;
//! every way the data directory can be broken ends in a [`LoadOutcome`].
use super::{
    atomic::atomic_json,
    meta::{
        next_crash_count, parse_meta, Fingerprint, PreviousRevision, RunMarker, StoreMeta,
        CRASH_LOOP_THRESHOLD, META_FILE,
    },
};
use crate::{
    error::{file_label, SafeModeReason},
    faults,
    startup::{ProblemKind, StartupProblem},
};
use quicker_domain::*;
use quicker_platform::{
    fault::FaultPoint,
    traits::{Clock, SystemClock},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

/// Backoff between reads that fail with an I/O error (sharing violation,
/// permissions, a cloud placeholder, antivirus): an I/O failure is not
/// corruption.
const RETRY_DELAYS_MS: [u64; 4] = [100, 200, 400, 800];

/// File access used while loading; tests substitute locks and failures.
pub trait StoreIo: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// The names of the entries in `dir`. An entry that cannot be read fails
    /// the whole listing, so a partial listing is never taken for complete.
    fn list_dir(&self, dir: &Path) -> io::Result<Vec<OsString>>;
}

/// The real file system. `fs::read` opens with every share mode on Windows,
/// so reading never blocks another process that has the file open.
pub struct FsIo;
impl StoreIo for FsIo {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if path.file_name().is_some_and(|name| name == "active.json")
            && faults::peek(FaultPoint::StoreLocked).is_some()
        {
            return Err(io::Error::other("injected store_locked"));
        }
        fs::read(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
    fn list_dir(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }
}

/// Everything `load_or_fallback` needs from the outside world.
pub struct StoreEnv {
    pub io: Box<dyn StoreIo>,
    pub clock: Box<dyn Clock>,
    /// Identifies the current boot; see [`super::meta::system_boot_id`].
    pub boot_id: Option<u64>,
    pub pid: u32,
}
impl StoreEnv {
    pub fn system() -> Self {
        let clock = SystemClock;
        let boot_id = super::meta::system_boot_id(clock.wall_ms());
        Self {
            io: Box::new(FsIo),
            clock: Box::new(clock),
            boot_id,
            pid: std::process::id(),
        }
    }
}

/// Where a recovered configuration came from (`recovered.args.from`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoverySource {
    LastGood,
    Revision,
    Previous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    /// `active.json` was read (or there is no data yet).
    Normal,
    /// `active.json` was unusable; another candidate was loaded.
    Recovered {
        from: RecoverySource,
        revision: Option<u64>,
    },
    SafeMode(SafeModeReason),
}

/// The result of [`load_or_fallback`].
pub struct LoadReport {
    pub outcome: LoadOutcome,
    pub active: Config,
    /// Whether `active` was read from a file (`active.json` or a fallback
    /// candidate). False for the empty configuration a new data directory,
    /// SafeMode(NoValidConfig) and the read-only safe modes start with: it
    /// was never on disk, so it must never be backed up as a revision.
    pub active_on_disk: bool,
    /// `active` compiled rule by rule; rules that failed stay off (B-43).
    pub compiled: CompiledConfig,
    pub draft: Config,
    pub previous: Option<Config>,
    /// SHA-256 of `active.json` as read; `None` when it was not the source.
    pub disk_hash: Option<Vec<u8>>,
    /// The `safe_mode` problem, when present, comes first.
    pub problems: Vec<StartupProblem>,
    pub meta: StoreMeta,
    /// Whether `store-meta.json` may be written this session.
    pub meta_writable: bool,
    /// Corrupt files left where they are until the pre-0.4 backup holds
    /// them (review I-B1, see [`DeferredQuarantine`]).
    pub deferred: Vec<DeferredQuarantine>,
}

/// A corrupt configuration file that the load did not rename (review I-B1):
/// while `store-meta.json` does not say the pre-0.4 backup has been dealt
/// with, nothing that 0.3.1 reads may change, not even by a quarantine
/// rename. The load goes on as if the file were quarantined; the store
/// renames it once the backup holds it (`Store::quarantine_deferred`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredQuarantine {
    pub path: PathBuf,
    /// Of the bytes found corrupt: a file that changed since is left alone.
    pub fingerprint: Fingerprint,
    /// When the load found it, for the quarantine name.
    pub found_ms: i64,
}
impl LoadReport {
    pub fn safe_mode(&self) -> Option<SafeModeReason> {
        match self.outcome {
            LoadOutcome::SafeMode(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Loads the data directory (plan §7.2 `load_or_fallback`):
///
/// 1. Reads `store-meta.json`; a run marker left in the same boot less than
///    5 minutes ago counts one more consecutive crash.
/// 2. Tries `active.json`, then the last-known-good revision, then
///    `revisions/<n>.json` newest first, then `previous.json`. Reads that
///    fail with an I/O error are retried with backoff. `active.json` still
///    unreadable is SafeMode(Locked): no fallback, no quarantine, no write;
///    another candidate that stays unreadable is skipped.
/// 3. Only content that does not parse is quarantined (renamed to
///    `<name>.corrupt-<time>.json`); if that rename fails nothing may
///    overwrite the file, so `active.json` and `draft.json` then also mean
///    SafeMode(Locked). A newer schema is SafeMode(NewerSchema): read-only.
///    Until `store-meta.json` says the pre-0.4 backup has been dealt with
///    (absent or unreadable mark), the rename waits for that backup: the
///    file stays in place and is listed in [`LoadReport::deferred`]
///    (review I-B1).
/// 4. The chosen configuration is read leniently and compiled rule by rule.
///    No usable candidate at all is SafeMode(NoValidConfig) with an empty
///    configuration, also on later launches while the candidates an earlier
///    one quarantined are all that is left, or while a directory that could
///    hold candidates cannot be listed; two or more consecutive crashes
///    are SafeMode(CrashLoop) with the data loaded for viewing.
/// 5. Outside the read-only safe modes `store-meta.json` gets the new run
///    marker. One that did not parse is quarantined first; when that rename
///    fails, or the file could not be read, it is not written this session
///    and the marker and crash count stay in memory.
pub fn load_or_fallback(dir: &Path, env: &StoreEnv) -> LoadReport {
    let mut loader = Loader::new(dir, env);
    let (meta, meta_file) = loader.read_meta();
    // I-B1: an unreadable or rebuilt store-meta.json says nothing either.
    loader.defer = meta.backup_pre_0_4.is_none();
    // Review R7-3, diagnostic only: once the mark is set the backup is not
    // checked again, so a recorded directory that went away is only logged.
    if let (Some(true), Some(name)) = (meta.backup_pre_0_4, &meta.backup_pre_0_4_dir) {
        let recorded = Path::new(name);
        if matches!(dir.join(recorded).try_exists(), Ok(false)) {
            tracing::warn!(event = "store.backup_missing", dir = %label(recorded), "recorded pre-0.4 backup directory is missing");
        }
    }
    let now = RunMarker {
        boot_id: env.boot_id,
        started_ms: env.clock.wall_ms(),
        pid: env.pid,
    };
    let crash_count = next_crash_count(&meta, &now);

    let active_path = dir.join("active.json");
    let mut disk_hash = None;
    let mut loaded_fingerprint = None;
    let mut found = None;
    let mut any_candidate = false;
    match loader.candidate(&active_path) {
        Candidate::Loaded(stored, bytes) => {
            disk_hash = Some(Sha256::digest(&bytes).to_vec());
            loaded_fingerprint = Some(Fingerprint::of(&bytes));
            found = Some((*stored, LoadOutcome::Normal));
        }
        Candidate::Missing => {}
        Candidate::Corrupt { quarantined: true } => any_candidate = true,
        Candidate::Unreadable | Candidate::Corrupt { quarantined: false } => {
            return loader.read_only(SafeModeReason::Locked, meta)
        }
        Candidate::Newer => return loader.read_only(SafeModeReason::NewerSchema, meta),
    }
    if found.is_none() {
        let (candidates, listed) = loader.fallback_candidates(meta.last_good_revision);
        // Revisions that could not be listed may hold candidates: like a
        // candidate that could not be read, that is no new data directory.
        any_candidate |= !listed;
        for (path, from) in candidates {
            match loader.candidate(&path) {
                Candidate::Loaded(stored, _) => {
                    let revision =
                        (from != RecoverySource::Previous).then_some(stored.config.revision);
                    tracing::warn!(
                        event = "store.recovered",
                        file = %label(&path),
                        ?from,
                        ?revision,
                        "active.json unusable; recovered from another file"
                    );
                    loader.problem(ProblemKind::Recovered { from, revision });
                    found = Some((*stored, LoadOutcome::Recovered { from, revision }));
                    break;
                }
                Candidate::Missing => {}
                Candidate::Unreadable | Candidate::Corrupt { .. } => any_candidate = true,
                Candidate::Newer => return loader.read_only(SafeModeReason::NewerSchema, meta),
            }
        }
    }
    let active_on_disk = found.is_some();
    let (stored, mut outcome) = match found {
        Some(found) => found,
        None => {
            let stored = StoredConfig {
                config: Config::default(),
                diagnostics: vec![],
            };
            // F4-1: candidates an earlier launch quarantined still count, so
            // SafeMode(NoValidConfig) lasts until an import is published
            // (which writes active.json). Derived from the directory, not
            // from a flag in store-meta.json, which may be unreadable or
            // left unwritten this session.
            if !any_candidate {
                match loader.quarantined_candidates() {
                    Ok(0) => {}
                    Ok(quarantined) => {
                        tracing::warn!(
                            event = "store.no_valid_config_kept",
                            quarantined,
                            "no configuration to load; candidates quarantined by an earlier launch"
                        );
                        any_candidate = true;
                    }
                    // R2: evidence that could not be inspected (logged as
                    // `store.list_failed`) fails closed, never as a new
                    // data directory.
                    Err(_) => any_candidate = true,
                }
            }
            if any_candidate {
                (stored, LoadOutcome::SafeMode(SafeModeReason::NoValidConfig))
            } else {
                // A new data directory.
                (stored, LoadOutcome::Normal)
            }
        }
    };
    log_read_diagnostics(&stored.diagnostics);
    let active = stored.config;

    let draft = match loader.candidate(&dir.join("draft.json")) {
        Candidate::Loaded(stored, _) => stored.config,
        Candidate::Missing | Candidate::Corrupt { quarantined: true } => active.clone(),
        // An unpublished draft we could not read must not be overwritten.
        Candidate::Unreadable | Candidate::Corrupt { quarantined: false } => {
            return loader.read_only(SafeModeReason::Locked, meta)
        }
        Candidate::Newer => return loader.read_only(SafeModeReason::NewerSchema, meta),
    };
    let previous = loader.previous(
        &active,
        &meta.previous_revisions,
        loaded_fingerprint.as_ref(),
    );

    // Rules that did not compile plus entries that did not read; entries
    // without a string id count but have no id to list.
    let (compiled, mut skipped) = compile_lenient(&active);
    let count = skipped.len() + active.unreadable_rules.len();
    skipped.extend(
        active
            .unreadable_rules
            .iter()
            .filter_map(|entry| entry.id().map(str::to_string)),
    );
    if count > 0 {
        tracing::warn!(
            event = "store.rules_skipped",
            count,
            "rules stay off at runtime"
        );
        loader.problem(ProblemKind::RulesSkipped {
            count,
            ids: skipped,
        });
    }
    let keys = active.unsupported_terminators();
    if !keys.is_empty() {
        loader.problem(ProblemKind::TerminatorUnsupported { keys });
    }

    // A data problem is the more specific reason.
    if !matches!(outcome, LoadOutcome::SafeMode(_)) && crash_count >= CRASH_LOOP_THRESHOLD {
        tracing::warn!(
            event = "store.crash_loop",
            crash_count,
            "consecutive crashes at startup"
        );
        outcome = LoadOutcome::SafeMode(SafeModeReason::CrashLoop);
    }
    let mut meta = meta;
    meta.run_marker = Some(now);
    meta.crash_count = crash_count;
    let meta_writable = match meta_file {
        MetaFile::Usable => true,
        MetaFile::Unreadable => false,
        MetaFile::Corrupt => loader.quarantine_meta(),
    };
    if meta_writable {
        if let Err(e) = atomic_json(&dir.join(META_FILE), &meta) {
            tracing::warn!(event = "store.meta_write_failed", error = %e, "store-meta.json not written");
        }
    } else {
        tracing::warn!(
            event = "store.meta_readonly",
            "store-meta.json left untouched; run marker and crash count kept in memory"
        );
    }
    let mut problems = loader.problems;
    if let LoadOutcome::SafeMode(reason) = outcome {
        problems.insert(0, ProblemKind::SafeMode { reason }.into());
    }
    LoadReport {
        outcome,
        active,
        active_on_disk,
        compiled,
        draft,
        previous,
        disk_hash,
        problems,
        meta,
        meta_writable,
        deferred: loader.deferred,
    }
}

/// One more attempt while in SafeMode(Locked); `None` while still locked.
pub fn retry_locked(dir: &Path, env: &StoreEnv) -> Option<LoadReport> {
    let report = load_or_fallback(dir, env);
    (report.outcome != LoadOutcome::SafeMode(SafeModeReason::Locked)).then_some(report)
}

enum Candidate {
    Loaded(Box<StoredConfig>, Vec<u8>),
    Missing,
    /// Still failing with an I/O error after the retries.
    Unreadable,
    /// Did not parse; `quarantined` tells whether the rename succeeded.
    Corrupt {
        quarantined: bool,
    },
    Newer,
}

enum Read {
    Bytes(Vec<u8>),
    Missing,
    Failed,
}

/// What `store-meta.json` allows this session.
enum MetaFile {
    /// Parsed, or not there yet: it may be written.
    Usable,
    /// Reading failed: never replaced this session.
    Unreadable,
    /// Did not parse: replaced only once the original is quarantined.
    Corrupt,
}

struct Loader<'a> {
    dir: &'a Path,
    env: &'a StoreEnv,
    problems: Vec<StartupProblem>,
    /// Corrupt files are listed in `deferred` instead of renamed (I-B1).
    defer: bool,
    deferred: Vec<DeferredQuarantine>,
}
impl<'a> Loader<'a> {
    fn new(dir: &'a Path, env: &'a StoreEnv) -> Self {
        Self {
            dir,
            env,
            problems: vec![],
            defer: false,
            deferred: vec![],
        }
    }

    fn problem(&mut self, kind: ProblemKind) {
        self.problems.push(kind.into());
    }

    fn read(&self, path: &Path) -> Read {
        let mut delays = RETRY_DELAYS_MS.iter();
        loop {
            match self.env.io.read(path) {
                Ok(bytes) => return Read::Bytes(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Read::Missing,
                Err(e) => match delays.next() {
                    Some(ms) => self.env.clock.sleep(Duration::from_millis(*ms)),
                    None => {
                        tracing::warn!(
                            event = "store.read_failed",
                            file = %label(path),
                            error = %e,
                            "file stayed unreadable after retries"
                        );
                        return Read::Failed;
                    }
                },
            }
        }
    }

    /// Reads `store-meta.json`; nothing is renamed or written here, so the
    /// read-only safe modes leave a corrupt file exactly as it is.
    fn read_meta(&self) -> (StoreMeta, MetaFile) {
        match self.read(&self.dir.join(META_FILE)) {
            Read::Missing => (StoreMeta::default(), MetaFile::Usable),
            Read::Failed => (StoreMeta::default(), MetaFile::Unreadable),
            Read::Bytes(bytes) => match parse_meta(&bytes) {
                Some(meta) => (meta, MetaFile::Usable),
                None => (StoreMeta::default(), MetaFile::Corrupt),
            },
        }
    }

    /// Moves a corrupt `store-meta.json` aside so that the new default one
    /// does not destroy it. False when the rename failed: the file must then
    /// stay untouched for the rest of the session.
    fn quarantine_meta(&mut self) -> bool {
        let path = self.dir.join(META_FILE);
        if self.rename_aside(&path).is_none() {
            return false;
        }
        tracing::warn!(
            event = "store.meta_reset",
            "store-meta.json did not parse; quarantined and rebuilt"
        );
        self.problem(ProblemKind::StoreMetaReset {});
        true
    }

    fn candidate(&mut self, path: &Path) -> Candidate {
        let bytes = match self.read(path) {
            Read::Bytes(bytes) => bytes,
            Read::Missing => return Candidate::Missing,
            Read::Failed => return Candidate::Unreadable,
        };
        match read_stored_config(&bytes) {
            Ok(stored) => Candidate::Loaded(Box::new(stored), bytes),
            Err(StoredConfigError::NewerSchema { version }) => {
                tracing::warn!(
                    event = "store.newer_schema",
                    file = %label(path),
                    version,
                    "written by a newer release; opening read-only"
                );
                Candidate::Newer
            }
            Err(e) => {
                // The diagnostics bundle shows the same kind, line and column.
                let span = e.diagnostic().span;
                tracing::warn!(
                    event = "store.corrupt",
                    file = %label(path),
                    code = e.code(),
                    kind = e.malformed_kind().map(MalformedKind::as_str),
                    line = span.line,
                    column = span.column,
                    error = %e,
                    "configuration file does not parse"
                );
                Candidate::Corrupt {
                    quarantined: self.quarantine(path, &bytes),
                }
            }
        }
    }

    /// Quarantines a corrupt configuration file and reports it. Before the
    /// pre-0.4 backup has been dealt with the file is only listed for the
    /// store to rename later, and counts as quarantined here.
    fn quarantine(&mut self, path: &Path, bytes: &[u8]) -> bool {
        if self.defer {
            tracing::warn!(
                event = "store.quarantine_deferred",
                file = %label(path),
                "corrupt file left in place until the pre-0.4 backup holds it"
            );
            self.deferred.push(DeferredQuarantine {
                path: path.to_path_buf(),
                fingerprint: Fingerprint::of(bytes),
                found_ms: self.env.clock.wall_ms(),
            });
            return true;
        }
        let Some(target) = self.rename_aside(path) else {
            return false;
        };
        self.problem(ProblemKind::ConfigQuarantined {
            file: label(path),
            quarantined_as: label(&target),
        });
        true
    }

    /// Renames a corrupt file to `<stem>.corrupt-<UTC time>.json` beside it;
    /// `None` when the rename failed.
    fn rename_aside(&self, path: &Path) -> Option<PathBuf> {
        let target = quarantine_target(path, self.env.clock.wall_ms());
        match self.env.io.rename(path, &target) {
            Ok(()) => {
                tracing::warn!(
                    event = "store.quarantine",
                    file = %label(path),
                    quarantined_as = %label(&target),
                    "quarantined corrupt file"
                );
                Some(target)
            }
            Err(e) => {
                tracing::warn!(
                    event = "store.quarantine_failed",
                    file = %label(path),
                    error = %e,
                    "corrupt file could not be renamed; it will not be overwritten"
                );
                None
            }
        }
    }

    /// The names in `dir`. A directory that does not exist is empty; any
    /// other failure, of the listing or of one entry, is logged and returned.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        match self.env.io.list_dir(dir) {
            // Names that are not UTF-8 are no store file names.
            Ok(names) => Ok(names
                .into_iter()
                .filter_map(|name| name.into_string().ok())
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => {
                tracing::warn!(
                    event = "store.list_failed",
                    dir = %label(dir),
                    kind = %e.kind(),
                    os_code = ?e.raw_os_error(),
                    "store directory could not be listed"
                );
                Err(e)
            }
        }
    }

    /// The last-known-good revision, then the other revisions newest first,
    /// then `previous.json`; false when `revisions` could not be listed, so
    /// the list may miss some.
    fn fallback_candidates(
        &self,
        last_good: Option<u64>,
    ) -> (Vec<(PathBuf, RecoverySource)>, bool) {
        let revisions = self.dir.join("revisions");
        let mut candidates = vec![];
        if let Some(n) = last_good {
            candidates.push((
                revisions.join(format!("{n}.json")),
                RecoverySource::LastGood,
            ));
        }
        let listing = self.list(&revisions);
        let listed = listing.is_ok();
        let mut numbers: Vec<u64> = listing
            .unwrap_or_default()
            .iter()
            .filter_map(|name| revision_number(name))
            .collect();
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        candidates.extend(
            numbers
                .into_iter()
                .filter(|n| Some(*n) != last_good)
                .map(|n| {
                    (
                        revisions.join(format!("{n}.json")),
                        RecoverySource::Revision,
                    )
                }),
        );
        candidates.push((self.dir.join("previous.json"), RecoverySource::Previous));
        (candidates, listed)
    }

    /// How many configuration candidates an earlier launch quarantined (F4-1):
    /// `active` and `previous` beside `store-meta.json`, `revisions/<n>` below
    /// it, renamed by [`quarantine_target`]. The quarantines of `draft.json`,
    /// `store-meta.json` and `prefs.json` are not candidates. An error when
    /// the directories cannot be listed (R2): the caller must not read it as
    /// "none".
    fn quarantined_candidates(&self) -> io::Result<usize> {
        let top = self
            .list(self.dir)?
            .iter()
            .filter(|name| matches!(quarantined_stem(name), Some("active" | "previous")))
            .count();
        let revisions = self
            .list(&self.dir.join("revisions"))?
            .iter()
            .filter(|name| quarantined_stem(name).is_some_and(is_revision_stem))
            .count();
        Ok(top + revisions)
    }

    /// The configuration `rollback` returns to. Not a fallback candidate: a
    /// file that does not read only makes rollback unavailable.
    ///
    /// The predecessor the publish of `active` recorded, so that a restart
    /// keeps the rollback target of the session before (R3); `None` when it
    /// replaced the empty stand-in. A record applies only to the bytes of
    /// `active.json` this load read (`loaded`, review I-R2): its revision and
    /// fingerprint must both match, so a reused revision number, or a
    /// configuration recovered from a fallback candidate (`loaded` is
    /// `None`), never takes another revision's record. Without a record
    /// that applies (a meta file from before it existed, not written or
    /// rebuilt, or a revision published by 0.3.1), or when the recorded
    /// snapshot is gone, the predecessor is guessed: the nearest snapshot
    /// below the active revision, which is `revisions/<active - 1>.json`
    /// whenever that is there (as before), else the next one down, since
    /// a publish skips the numbers other files in `revisions/` use (review
    /// R7-2); with none, `previous.json`.
    fn previous(
        &self,
        active: &Config,
        records: &[PreviousRevision],
        loaded: Option<&Fingerprint>,
    ) -> Option<Config> {
        let revisions = self.dir.join("revisions");
        let record = loaded.and_then(|loaded| {
            records
                .iter()
                .find(|record| record.active == active.revision && &record.fingerprint == loaded)
        });
        let recorded = match record {
            Some(record) => {
                let n = record.revision?;
                Some(revisions.join(format!("{n}.json"))).filter(|path| path.exists())
            }
            None => None,
        };
        let path = recorded.unwrap_or_else(|| {
            revision_numbers(self.dir)
                .into_iter()
                .filter(|&n| n < active.revision)
                .max()
                .map_or_else(
                    || self.dir.join("previous.json"),
                    |n| revisions.join(format!("{n}.json")),
                )
        });
        let Read::Bytes(bytes) = self.read(&path) else {
            return None;
        };
        match read_stored_config(&bytes) {
            Ok(stored) => Some(stored.config),
            Err(e) => {
                tracing::warn!(
                    event = "store.previous_unreadable",
                    file = %label(&path),
                    code = e.code(),
                    "previous configuration unavailable for rollback"
                );
                None
            }
        }
    }

    /// SafeMode(Locked) or SafeMode(NewerSchema): nothing is loaded and
    /// nothing is written; `store-meta.json` is neither written nor renamed,
    /// and corrupt files found so far stay where they are.
    fn read_only(self, reason: SafeModeReason, meta: StoreMeta) -> LoadReport {
        let active = Config::default();
        let (compiled, _) = compile_lenient(&active);
        let mut problems = vec![ProblemKind::SafeMode { reason }.into()];
        problems.extend(self.problems);
        LoadReport {
            outcome: LoadOutcome::SafeMode(reason),
            draft: active.clone(),
            compiled,
            active,
            active_on_disk: false,
            previous: None,
            disk_hash: None,
            problems,
            meta,
            meta_writable: false,
            deferred: vec![],
        }
    }
}

/// Compiles rule by rule. A configuration-level error (an empty global
/// terminator) leaves every enabled rule off.
fn compile_lenient(config: &Config) -> (CompiledConfig, Vec<String>) {
    match compile_config_lenient(config) {
        Ok(lenient) => {
            if !lenient.diagnostics.is_empty() {
                log_read_diagnostics(&lenient.diagnostics);
            }
            (lenient.compiled, lenient.skipped_rule_ids)
        }
        Err(diagnostics) => {
            log_read_diagnostics(&diagnostics);
            let skipped = config
                .rules
                .iter()
                .filter(|rule| rule.enabled)
                .map(|rule| rule.id.clone())
                .collect();
            let compiled = CompiledConfig {
                config: config.clone(),
                rules: vec![],
            };
            (compiled, skipped)
        }
    }
}

/// Codes and paths only: messages may echo configuration values.
fn log_read_diagnostics(diagnostics: &[Diagnostic]) {
    for d in diagnostics {
        tracing::warn!(event = "store.diagnostic", code = %d.code, path = %d.path, "stored configuration diagnostic");
    }
}

fn label(path: &Path) -> String {
    file_label(path).unwrap_or_default()
}

/// Revision numbers that have a `revisions/<n>.json` file.
pub(super) fn revision_numbers(dir: &Path) -> Vec<u64> {
    let Ok(entries) = fs::read_dir(dir.join("revisions")) else {
        return vec![];
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| revision_number(entry.file_name().to_str()?))
        .collect()
}

/// `n` for the name `<n>.json` of a revision snapshot.
fn revision_number(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".json")?;
    if !is_revision_stem(stem) {
        return None;
    }
    stem.parse().ok()
}

fn is_revision_stem(stem: &str) -> bool {
    !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit())
}

pub(super) fn quarantine_target(path: &Path, wall_ms: i64) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stamp = utc_stamp(wall_ms);
    let mut target = path.with_file_name(format!("{stem}.corrupt-{stamp}.json"));
    let mut n = 1;
    while target.exists() {
        target = path.with_file_name(format!("{stem}.corrupt-{stamp}-{n}.json"));
        n += 1;
    }
    target
}

/// The stem of a name [`quarantine_target`] produces,
/// `<stem>.corrupt-<yyyymmdd-hhmmss>[-<n>].json`; `None` for any other name.
fn quarantined_stem(name: &str) -> Option<&str> {
    let (stem, rest) = name.strip_suffix(".json")?.split_once(".corrupt-")?;
    let digits = |bytes: &[u8]| !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit);
    let rest = rest.as_bytes();
    let stamp = rest.len() >= 15 && digits(&rest[..8]) && rest[8] == b'-' && digits(&rest[9..15]);
    let suffix = &rest[rest.len().min(15)..];
    let suffix = suffix.is_empty() || suffix.strip_prefix(b"-").is_some_and(digits);
    (stamp && suffix).then_some(stem)
}

/// `yyyymmdd-hhmmss` in UTC (same format as the prefs quarantine).
fn utc_stamp(wall_ms: i64) -> String {
    let secs = wall_ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F4-1: every name `quarantine_target` produces for a candidate is
    /// recognised, collision suffix included; other names are not.
    #[test]
    fn quarantined_candidates_match_the_quarantine_names() {
        let dir = std::env::temp_dir().join(format!("quicker-quarantine-{}", uuid::Uuid::new_v4()));
        let env = StoreEnv::system();
        let quarantined_candidates =
            |dir: &Path| Loader::new(dir, &env).quarantined_candidates().unwrap();
        // A directory that does not exist (yet) is empty (R2).
        assert_eq!(quarantined_candidates(&dir), 0);
        fs::create_dir_all(dir.join("revisions")).unwrap();
        assert_eq!(quarantined_candidates(&dir), 0);
        // Not candidates, or not names the quarantine produces.
        for name in [
            "active.json",
            "draft.corrupt-20261001-000000.json",
            "store-meta.corrupt-20261001-000000.json",
            "prefs.corrupt-20261001-000000.json",
            "active.corrupt-.json",
            "active.corrupt-2026-01-01.json",
            "active.corrupt-20261001-000000-.json",
            "active.corrupt-20261001-000000-x.json",
            "active.corrupt-20261001-000000.txt",
            "revisions/1.json",
            "revisions/1.metadata.json",
            "revisions/x.corrupt-20261001-000000.json",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        assert_eq!(quarantined_candidates(&dir), 0);
        let wall_ms = 1_800_000_000_000;
        for (n, name) in ["active.json", "previous.json", "revisions/12.json"]
            .into_iter()
            .enumerate()
        {
            for _ in 0..2 {
                // The second one gets the `-1` suffix.
                let target = quarantine_target(&dir.join(name), wall_ms);
                fs::write(&target, "x").unwrap();
            }
            assert_eq!(quarantined_candidates(&dir), 2 * (n + 1), "{name}");
        }
        fs::remove_dir_all(dir).unwrap();
    }
}

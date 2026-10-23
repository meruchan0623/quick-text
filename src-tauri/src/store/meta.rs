//! `store-meta.json` (B-04): last-known-good revision, the predecessor of
//! the active revision, the run marker, the consecutive crash count and
//! whether the pre-0.4 backup has been dealt with. A
//! new file that 0.3.1 never reads (§9), so
//! nothing here touches `active.json` or `revisions/<n>.json`.
use super::atomic::atomic_json;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub const META_FILE: &str = "store-meta.json";
/// A marker left by a start less than this long ago counts as a crash.
pub const CRASH_WINDOW_MS: i64 = 5 * 60 * 1000;
/// This many consecutive crashes start in SafeMode(CrashLoop).
pub const CRASH_LOOP_THRESHOLD: u32 = 2;
/// Boot ids are derived from the wall clock, which may be adjusted while
/// the system runs; ids this close are the same boot.
const BOOT_ID_TOLERANCE_S: u64 = 10;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StoreMeta {
    pub last_good_revision: Option<u64>,
    /// What rollback returns to after a restart (R3), written by every
    /// publish before it replaces `active.json`: at most two records, the
    /// one of that publish first, then the one that described the revision
    /// active when it started (review I-R1), so that a publish that fails or
    /// is cut short after this write leaves the committed revision its own
    /// record. The load takes the record whose `active` and `fingerprint`
    /// match the `active.json` it read (review I-R2), and guesses as before
    /// without one. Absent in a file from before it existed. (A development
    /// build wrote a single `previous_revision` object instead; that field is
    /// no longer read and stays in `extra`.)
    pub previous_revisions: Vec<PreviousRevision>,
    pub run_marker: Option<RunMarker>,
    pub crash_count: u32,
    /// The backup before the first 0.4 write (R6, `store::backup`):
    /// `true` once a complete backup exists (made or found), `false` when
    /// there was nothing from before 0.4 to back up. Absent until then, and
    /// in a file from before it existed.
    pub backup_pre_0_4: Option<bool>,
    /// The directory of that backup in the data directory (review I-B3):
    /// `backup-pre-0.4`, or `backup-pre-0.4-<UTC time>` when an incomplete
    /// `backup-pre-0.4` was in the way. Set with `backup_pre_0_4: true`;
    /// absent otherwise and in a file from before it existed.
    pub backup_pre_0_4_dir: Option<String>,
    /// Fields written by a newer release, kept verbatim (B-43).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The revision a publish replaced: the one rollback returns to. Recorded
/// because it is a fact that `active - 1` only guesses: revision 3
/// published over revision 1 recovered as last-known-good is not preceded
/// by revision 2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousRevision {
    /// The revision that was published. Any other active revision (one
    /// published by 0.3.1, which never writes this file, or one recovered
    /// from a fallback) is not described by this entry.
    pub active: u64,
    /// `revisions/<n>.json`; `None` when `active` replaced the empty
    /// stand-in, which is never backed up, so there is nothing to return to.
    pub revision: Option<u64>,
    /// Of the `active.json` that publish wrote. A revision number alone can
    /// be reused (a lost revision recovered from an older one, with this
    /// file unreadable meanwhile): the record applies only to those bytes.
    pub fingerprint: Fingerprint,
}

/// Identifies file content: its length and SHA-256 (lowercase hex). Used for
/// the records above and the manifest of the pre-0.4 backup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub len: u64,
    pub sha256: String,
}
impl Fingerprint {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            len: bytes.len() as u64,
            sha256: Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        }
    }
}

/// Written at startup and cleared on a clean exit or session end; one still
/// present at the next start means the previous run did not end cleanly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunMarker {
    /// Boot time in Unix seconds; `None` where it is not available.
    pub boot_id: Option<u64>,
    pub started_ms: i64,
    pub pid: u32,
}

/// Lenient parse: a UTF-8 BOM is skipped, missing fields default and unknown
/// ones stay in `extra`. `None` when the content cannot be parsed.
pub fn parse_meta(bytes: &[u8]) -> Option<StoreMeta> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    serde_json::from_slice(bytes).ok()
}

fn same_boot(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= BOOT_ID_TOLERANCE_S,
        // Without boot ids only the time window applies.
        _ => true,
    }
}

/// The crash count for a start described by `now`. A marker left in the
/// same boot less than [`CRASH_WINDOW_MS`] earlier adds one; no marker (the
/// previous run ended cleanly or with the session), a marker from another
/// boot, or an older one starts over at zero.
pub fn next_crash_count(meta: &StoreMeta, now: &RunMarker) -> u32 {
    match &meta.run_marker {
        Some(previous)
            if same_boot(previous.boot_id, now.boot_id)
                && (0..CRASH_WINDOW_MS).contains(&(now.started_ms - previous.started_ms)) =>
        {
            meta.crash_count.saturating_add(1)
        }
        _ => 0,
    }
}

/// Boot time in Unix seconds from `GetTickCount64` (which counts sleep and
/// hibernation), rounded to the second.
pub fn system_boot_id(wall_ms: i64) -> Option<u64> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetTickCount64() -> u64;
        }
        // SAFETY: no arguments, no preconditions.
        let uptime_ms = i64::try_from(unsafe { GetTickCount64() }).ok()?;
        u64::try_from((wall_ms - uptime_ms + 500) / 1000).ok()
    }
    #[cfg(not(windows))]
    {
        let _ = wall_ms;
        None
    }
}

/// The store's copy of `store-meta.json`, shared with the exit and
/// session-end paths so they never wait on the store lock.
#[derive(Clone)]
pub struct MetaHandle(Arc<parking_lot::Mutex<MetaState>>);
struct MetaState {
    path: PathBuf,
    meta: StoreMeta,
    /// False while the store must not write (SafeMode(Locked) or
    /// SafeMode(NewerSchema), or the file could not be read).
    writable: bool,
}

impl MetaHandle {
    pub fn new(dir: &Path, meta: StoreMeta, writable: bool) -> Self {
        Self(Arc::new(parking_lot::Mutex::new(MetaState {
            path: dir.join(META_FILE),
            meta,
            writable,
        })))
    }
    #[cfg(test)]
    pub fn get(&self) -> StoreMeta {
        self.0.lock().meta.clone()
    }
    /// Reads the content without writing it.
    pub fn read<R>(&self, read: impl FnOnce(&StoreMeta) -> R) -> R {
        read(&self.0.lock().meta)
    }
    /// Replaces the content (after a reload) without writing it.
    pub fn replace(&self, meta: StoreMeta, writable: bool) {
        let mut state = self.0.lock();
        state.meta = meta;
        state.writable = writable;
    }
    /// Applies `change` and writes the file when the store may write.
    pub fn update(
        &self,
        change: impl FnOnce(&mut StoreMeta),
    ) -> Result<(), crate::error::CommandError> {
        let mut state = self.0.lock();
        change(&mut state.meta);
        if !state.writable {
            return Ok(());
        }
        atomic_json(&state.path, &state.meta)
    }
    /// Removes this run's marker from the file. Nothing happens when this
    /// run never wrote one (read-only safe mode) or it is already cleared.
    pub fn clear_run_marker(&self) {
        let mut state = self.0.lock();
        if !state.writable || state.meta.run_marker.is_none() {
            return;
        }
        state.meta.run_marker = None;
        match atomic_json(&state.path, &state.meta) {
            Ok(()) => tracing::info!(event = "store.run_marker_cleared", "run marker cleared"),
            Err(e) => tracing::warn!(
                event = "store.run_marker_clear_failed",
                error = %e,
                "run marker could not be cleared"
            ),
        }
    }
}

static SESSION_META: parking_lot::Mutex<Option<MetaHandle>> = parking_lot::Mutex::new(None);

/// Makes the running store's meta reachable from
/// [`clear_run_marker_on_session_end`]. Called from startup.
pub fn register_session_meta(meta: MetaHandle) {
    *SESSION_META.lock() = Some(meta);
}

/// Clears the run marker so that a shutdown is not counted as a crash at the
/// next start. Safe from any thread and without the store lock: it is called
/// on `RunEvent::Exit` and by the session-end callback on the session window
/// thread at `WM_ENDSESSION` (`startup::end_session`, R5), which does not
/// depend on tao raising `RunEvent::Exit` for it.
pub fn clear_run_marker_on_session_end() {
    let meta = SESSION_META.lock().clone();
    if let Some(meta) = meta {
        meta.clear_run_marker();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(boot_id: Option<u64>, started_ms: i64) -> RunMarker {
        RunMarker {
            boot_id,
            started_ms,
            pid: 1,
        }
    }
    fn meta(previous: Option<RunMarker>, crash_count: u32) -> StoreMeta {
        StoreMeta {
            run_marker: previous,
            crash_count,
            ..StoreMeta::default()
        }
    }

    #[test]
    fn crash_count_branches() {
        let now = marker(Some(1_000), 10 * CRASH_WINDOW_MS);
        // Clean exit or session end: no marker.
        assert_eq!(next_crash_count(&meta(None, 1), &now), 0);
        // Same boot, within the window: one more.
        let recent = marker(Some(1_000), now.started_ms - 1_000);
        assert_eq!(next_crash_count(&meta(Some(recent.clone()), 0), &now), 1);
        assert_eq!(next_crash_count(&meta(Some(recent), 1), &now), 2);
        // Wall clock drift within the tolerance is still the same boot.
        let drift = marker(Some(1_000 + BOOT_ID_TOLERANCE_S), now.started_ms - 1_000);
        assert_eq!(next_crash_count(&meta(Some(drift), 1), &now), 2);
        // Left over from another boot.
        let other_boot = marker(Some(500), now.started_ms - 1_000);
        assert_eq!(next_crash_count(&meta(Some(other_boot), 1), &now), 0);
        // Exactly at or beyond the window, or in the future: starts over.
        let old = marker(Some(1_000), now.started_ms - CRASH_WINDOW_MS);
        assert_eq!(next_crash_count(&meta(Some(old), 1), &now), 0);
        let future = marker(Some(1_000), now.started_ms + 1);
        assert_eq!(next_crash_count(&meta(Some(future), 1), &now), 0);
        // Without boot ids only the window applies.
        let unknown = marker(None, now.started_ms - 1_000);
        assert_eq!(next_crash_count(&meta(Some(unknown), 0), &now), 1);
    }

    #[test]
    fn meta_round_trips_and_keeps_unknown_fields() {
        let parsed = parse_meta(
            b"\xEF\xBB\xBF{\"last_good_revision\": 4, \"crash_count\": 1, \
              \"run_marker\": {\"boot_id\": 7, \"started_ms\": 9, \"pid\": 3}, \"future\": [1]}",
        )
        .unwrap();
        assert_eq!(parsed.last_good_revision, Some(4));
        // A file from before the predecessor was recorded (R3) and the
        // backup mark (R6).
        assert!(parsed.previous_revisions.is_empty());
        assert_eq!(parsed.backup_pre_0_4, None);
        assert_eq!(parsed.backup_pre_0_4_dir, None);
        let expected = RunMarker {
            boot_id: Some(7),
            started_ms: 9,
            pid: 3,
        };
        assert_eq!(parsed.run_marker, Some(expected));
        assert_eq!(parsed.extra["future"], serde_json::json!([1]));
        let written = serde_json::to_value(&parsed).unwrap();
        assert_eq!(written["future"], serde_json::json!([1]));
        assert_eq!(written["run_marker"]["boot_id"], 7);
        assert!(parse_meta(b"{\"crash_count\": \"x\"}").is_none());
        assert!(parse_meta(b"{trunc").is_none());
        assert_eq!(parse_meta(b"{}"), Some(StoreMeta::default()));
        let record = serde_json::json!({
            "active": 3,
            "revision": 1,
            "fingerprint": {"len": 2, "sha256": "ab"}
        });
        let recorded = parse_meta(
            serde_json::json!({
                "previous_revisions": [record],
                "crash_count": 0,
                "backup_pre_0_4": true,
                "backup_pre_0_4_dir": "backup-pre-0.4-20260101-000000"
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(recorded.backup_pre_0_4, Some(true));
        assert_eq!(
            recorded.backup_pre_0_4_dir.as_deref(),
            Some("backup-pre-0.4-20260101-000000")
        );
        let previous = PreviousRevision {
            active: 3,
            revision: Some(1),
            fingerprint: Fingerprint {
                len: 2,
                sha256: "ab".into(),
            },
        };
        assert_eq!(recorded.previous_revisions, [previous]);
        assert!(recorded.extra.is_empty());
        let written = serde_json::to_value(&recorded).unwrap();
        assert_eq!(written["previous_revisions"], serde_json::json!([record]));
        assert_eq!(written["backup_pre_0_4"], true);
        // The single record of a development build is kept but not read.
        let old = parse_meta(b"{\"previous_revision\": {\"active\": 3, \"revision\": 1}}").unwrap();
        assert!(old.previous_revisions.is_empty());
        assert_eq!(
            old.extra["previous_revision"],
            serde_json::json!({"active": 3, "revision": 1})
        );
    }

    /// Review I-R2: the fingerprint is the length and the SHA-256 of the
    /// bytes, the same on every run (unlike `DefaultHasher`).
    #[test]
    fn fingerprint_is_length_and_sha256() {
        assert_eq!(
            Fingerprint::of(b""),
            Fingerprint {
                len: 0,
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            }
        );
        assert_eq!(
            Fingerprint::of(b"abc"),
            Fingerprint {
                len: 3,
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn system_boot_id_is_stable() {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let a = system_boot_id(wall).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = system_boot_id(wall + 20).unwrap();
        assert!(same_boot(Some(a), Some(b)));
    }
}

//! Startup self-healing and safe mode (S-04 / B-04 acceptance, plan §7.2).
//! Every test works in its own temporary directory with a fake clock and,
//! where locks or failed renames are needed, fake file access.
use super::{
    atomic::atomic_json,
    export::{final_path, StageHooks},
    fallback::{load_or_fallback, retry_locked, LoadOutcome, LoadReport, StoreEnv, StoreIo},
    meta::{parse_meta, StoreMeta, META_FILE},
    test_support::*,
    RecoverySource, Store,
};
use crate::{
    autostart::LoginItem,
    error::{CommandError, SafeModeReason},
    startup::{defer_autostart_repair, ProblemKind},
};
use quicker_domain::*;
use quicker_platform::{health::PauseReasons, traits::Clock};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("quicker-startup-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.clone()
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// Shared state behind the fake clock and file access.
struct World {
    wall_ms: AtomicI64,
    sleeps: parking_lot::Mutex<Vec<u64>>,
    /// File names whose reads fail with an I/O error (another process holds them).
    locked: parking_lot::Mutex<Vec<&'static str>>,
    rename_fails: AtomicBool,
}
fn world() -> Arc<World> {
    Arc::new(World {
        wall_ms: AtomicI64::new(1_800_000_000_000),
        sleeps: parking_lot::Mutex::new(vec![]),
        locked: parking_lot::Mutex::new(vec![]),
        rename_fails: AtomicBool::new(false),
    })
}
impl World {
    fn advance(&self, ms: i64) {
        self.wall_ms.fetch_add(ms, Ordering::SeqCst);
    }
}
struct FakeIo(Arc<World>);
impl StoreIo for FakeIo {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let name = path.file_name().unwrap().to_string_lossy();
        if self.0.locked.lock().iter().any(|locked| *locked == name) {
            return Err(io::Error::other("sharing violation"));
        }
        fs::read(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.0.rename_fails.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        fs::rename(from, to)
    }
}
struct FakeClock(Arc<World>);
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn wall_ms(&self) -> i64 {
        self.0.wall_ms.load(Ordering::SeqCst)
    }
    fn sleep(&self, duration: Duration) {
        self.0.sleeps.lock().push(duration.as_millis() as u64);
    }
}
fn env_with_boot(world: &Arc<World>, boot_id: Option<u64>) -> StoreEnv {
    StoreEnv {
        io: Box::new(FakeIo(world.clone())),
        clock: Box::new(FakeClock(world.clone())),
        boot_id,
        pid: 4242,
    }
}
fn env(world: &Arc<World>) -> StoreEnv {
    env_with_boot(world, Some(1_000))
}

/// Every file below `dir` with its content.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}
/// No file was deleted: every original content still exists, at its own
/// path or under a quarantine name.
fn assert_nothing_lost(before: &BTreeMap<PathBuf, Vec<u8>>, after: &BTreeMap<PathBuf, Vec<u8>>) {
    for (path, content) in before {
        assert!(
            after.get(path) == Some(content) || after.values().any(|c| c == content),
            "{} was lost",
            path.display()
        );
    }
}
fn write(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
fn config_at(revision: u64) -> Config {
    Config {
        revision,
        ..metadata_config(&["a"])
    }
}
fn bytes(config: &Config) -> Vec<u8> {
    serde_json::to_vec_pretty(config).unwrap()
}
fn value_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec_pretty(value).unwrap()
}
fn kinds(report: &LoadReport) -> Vec<&ProblemKind> {
    report.problems.iter().map(|p| &p.kind).collect()
}
fn quarantined(problems: &[&ProblemKind]) -> Vec<String> {
    problems
        .iter()
        .filter_map(|kind| match kind {
            ProblemKind::ConfigQuarantined { file, .. } => Some(file.clone()),
            _ => None,
        })
        .collect()
}
fn meta_on_disk(dir: &Path) -> StoreMeta {
    parse_meta(&fs::read(dir.join(META_FILE)).unwrap()).unwrap()
}
const TRUNCATED: &[u8] = b"{\"version\": 1, \"rules\": [";

/// S-04 acceptance: the five corruption samples plus a BOM and a newer
/// version all start, and no file is deleted.
#[test]
fn corruption_samples_start_without_losing_files() {
    let recovered = LoadOutcome::Recovered {
        from: RecoverySource::Revision,
        revision: Some(1),
    };
    let mut regex = config_at(1);
    regex.rules[0].match_spec = MatchSpec {
        kind: MatchKind::Regex,
        value: "(".into(),
    };
    let mut wrong_type = serde_json::to_value(config_at(1)).unwrap();
    wrong_type["enabled"] = json!("yes");
    let mut newer = serde_json::to_value(config_at(1)).unwrap();
    newer["version"] = json!(SUPPORTED_SCHEMA_VERSION + 1);
    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend(bytes(&config_at(1)));
    let samples: Vec<(&str, Vec<u8>)> = vec![
        ("truncated", TRUNCATED.to_vec()),
        (
            "invalid utf-8",
            b"{\"version\": 1, \"rules\": [\"\xFF\xFE\"]}".to_vec(),
        ),
        ("wrong field type", value_bytes(&wrong_type)),
        ("regex does not compile", bytes(&regex)),
        ("corrupt revision file", bytes(&config_at(2))),
        ("bom", bom),
        ("newer version", value_bytes(&newer)),
    ];
    for (name, active) in samples {
        let dir = TempDir::new();
        let world = world();
        write(&dir, "active.json", &active);
        if name == "corrupt revision file" {
            // The revision rollback returns to, not a fallback candidate.
            write(&dir, "revisions/1.json", TRUNCATED);
        } else {
            write(&dir, "revisions/1.json", &bytes(&config_at(1)));
        }
        let before = files(&dir);
        let report = load_or_fallback(&dir, &env(&world));
        let after = files(&dir);
        assert_nothing_lost(&before, &after);
        let problems = kinds(&report);
        match name {
            "truncated" | "invalid utf-8" => {
                assert_eq!(report.outcome, recovered, "{name}");
                assert_eq!(report.active, config_at(1), "{name}");
                assert_eq!(quarantined(&problems), ["active.json"], "{name}");
                assert!(!dir.join("active.json").exists(), "{name}");
            }
            "wrong field type" => {
                assert_eq!(report.outcome, LoadOutcome::Normal);
                assert!(report.active.enabled);
                assert_eq!(report.active.rules, config_at(1).rules);
            }
            "regex does not compile" => {
                assert_eq!(report.outcome, LoadOutcome::Normal);
                assert_eq!(report.active, regex);
                assert!(report.compiled.rule("a").is_none());
                assert!(problems.contains(&&ProblemKind::RulesSkipped {
                    count: 1,
                    ids: vec!["a".into()],
                }));
            }
            "corrupt revision file" => {
                assert_eq!(report.outcome, LoadOutcome::Normal);
                assert_eq!(report.active.revision, 2);
                assert!(report.previous.is_none());
                assert!(quarantined(&problems).is_empty());
                assert_eq!(fs::read(dir.join("revisions/1.json")).unwrap(), TRUNCATED);
            }
            "bom" => {
                assert_eq!(report.outcome, LoadOutcome::Normal);
                assert_eq!(report.active, config_at(1));
            }
            "newer version" => {
                assert_eq!(
                    report.outcome,
                    LoadOutcome::SafeMode(SafeModeReason::NewerSchema)
                );
                assert_eq!(
                    problems[0],
                    &ProblemKind::SafeMode {
                        reason: SafeModeReason::NewerSchema
                    }
                );
                // Read-only: not even store-meta.json is written.
                assert_eq!(after, before);
            }
            _ => unreachable!(),
        }
        assert!(world.sleeps.lock().is_empty(), "{name}: no I/O retries");
    }
}

/// `active.json` held by another process: SafeMode(Locked) after the
/// retries, no fallback to an older revision and no write at all; once it
/// reads again the retry leaves safe mode.
#[test]
fn locked_active_is_read_only_until_it_unlocks() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(2)));
    write(&dir, "draft.json", &bytes(&config_at(2)));
    write(&dir, "revisions/1.json", &bytes(&config_at(1)));
    world.locked.lock().push("active.json");
    let before = files(&dir);

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(*world.sleeps.lock(), [100, 200, 400, 800]);
    assert_eq!(store.safe_mode(), Some(SafeModeReason::Locked));
    assert_eq!(
        store.startup_problems()[0].kind,
        ProblemKind::SafeMode {
            reason: SafeModeReason::Locked
        }
    );
    // Not recovered from revisions/1.json.
    assert_eq!(store.active, Config::default());
    assert!(store.engine.paused.load(Ordering::SeqCst));
    assert_eq!(
        store.runtime.pause_reasons(),
        PauseReasons::SAFE_MODE.with(PauseReasons::MANUAL)
    );
    // Even with the engine up, safe mode refuses a resume.
    store.runtime.publish_health(healthy());
    assert!(matches!(
        store.set_manual_pause(false, || true),
        Err(CommandError::SafeMode {
            reason: SafeModeReason::Locked
        })
    ));
    assert!(store.engine.paused.load(Ordering::SeqCst));
    assert!(store.save(config_at(5)).is_err());
    assert!(store.discard().is_err());
    assert!(store.publish(config_at(5)).is_err());
    store.meta.clear_run_marker();
    assert!(retry_locked(&dir, &env(&world)).is_none());
    assert_eq!(files(&dir), before, "nothing written while locked");

    world.locked.lock().clear();
    let report = retry_locked(&dir, &env(&world)).unwrap();
    assert_eq!(report.outcome, LoadOutcome::Normal);
    store.apply_retry(report);
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.active, config_at(2));
    assert!(store
        .startup_problems()
        .iter()
        .all(|p| !matches!(p.kind, ProblemKind::SafeMode { .. })));
    // Leaving safe mode never resumes the engine.
    assert!(store.engine.paused.load(Ordering::SeqCst));
    assert_eq!(store.runtime.pause_reasons(), PauseReasons::MANUAL);
    store.ensure_writable().unwrap();
    store.set_manual_pause(false, || true).unwrap();
    assert!(store.runtime.engine_state().listening);
    assert!(!store.engine.paused.load(Ordering::SeqCst));
}

/// A corrupt file that cannot be renamed must not be overwritten either.
#[test]
fn failed_quarantine_is_locked() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "revisions/1.json", &bytes(&config_at(1)));
    world.rename_fails.store(true, Ordering::SeqCst);
    let before = files(&dir);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(
        report.outcome,
        LoadOutcome::SafeMode(SafeModeReason::Locked)
    );
    assert!(!report.meta_writable);
    assert_eq!(files(&dir), before);
}

#[test]
fn no_valid_config_starts_empty_and_keeps_every_file() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "revisions/1.json", b"\xFF");
    write(&dir, "previous.json", b"[]");
    let before = files(&dir);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(
        report.outcome,
        LoadOutcome::SafeMode(SafeModeReason::NoValidConfig)
    );
    assert_eq!(report.active, Config::default());
    let mut names = quarantined(&kinds(&report));
    names.sort();
    assert_eq!(names, ["1.json", "active.json", "previous.json"]);
    assert_nothing_lost(&before, &files(&dir));
}

/// Fallback order: last-known-good, other revisions newest first, then
/// `previous.json`; publishing afterwards never reuses a revision number.
#[test]
fn recovers_in_plan_order_and_publishes_past_every_revision() {
    // Last-known-good first, even when a newer revision exists.
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "revisions/1.json", &bytes(&config_at(1)));
    write(&dir, "revisions/2.json", &bytes(&config_at(2)));
    let meta = StoreMeta {
        last_good_revision: Some(1),
        ..StoreMeta::default()
    };
    atomic_json(&dir.join(META_FILE), &meta).unwrap();
    let mut store = Store::load_with(dir.path(), &env(&world));
    let from_last_good = ProblemKind::Recovered {
        from: RecoverySource::LastGood,
        revision: Some(1),
    };
    assert!(store.startup_problems().contains(&from_last_good.into()));
    assert_eq!(store.active, config_at(1));
    assert_eq!(store.safe_mode(), None);
    let snapshot = fs::read(dir.join("revisions/2.json")).unwrap();
    store.publish(metadata_config(&["b"])).unwrap();
    assert_eq!(store.active.revision, 3);
    assert_eq!(fs::read(dir.join("revisions/2.json")).unwrap(), snapshot);

    // Newest revision that reads; corrupt ones are quarantined on the way.
    let dir = TempDir::new();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "revisions/1.json", &bytes(&config_at(1)));
    write(&dir, "revisions/2.json", TRUNCATED);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(
        report.outcome,
        LoadOutcome::Recovered {
            from: RecoverySource::Revision,
            revision: Some(1)
        }
    );
    assert_eq!(quarantined(&kinds(&report)), ["active.json", "2.json"]);

    // Then previous.json.
    let dir = TempDir::new();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "previous.json", &bytes(&config_at(4)));
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(
        report.outcome,
        LoadOutcome::Recovered {
            from: RecoverySource::Previous,
            revision: None
        }
    );
    assert_eq!(report.active, config_at(4));
}

#[test]
fn corrupt_draft_is_quarantined_and_restarts_from_active() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    write(&dir, "draft.json", TRUNCATED);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert_eq!(report.draft, config_at(1));
    assert_eq!(quarantined(&kinds(&report)), ["draft.json"]);
}

/// Unclean starts in the same boot within five minutes count; the second
/// consecutive one is SafeMode(CrashLoop) with the data loaded for viewing.
#[test]
fn consecutive_crashes_enter_crash_loop() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(3)));
    let first = Store::load_with(dir.path(), &env(&world));
    assert_eq!(first.safe_mode(), None);
    assert_eq!(meta_on_disk(&dir).crash_count, 0);
    assert!(meta_on_disk(&dir).run_marker.is_some());
    drop(first); // Killed: the marker stays.

    // A marker from another boot is not a crash.
    world.advance(1_000);
    let other_boot = Store::load_with(dir.path(), &env_with_boot(&world, Some(9_000)));
    assert_eq!(meta_on_disk(&dir).crash_count, 0);
    drop(other_boot);

    world.advance(1_000);
    let second = Store::load_with(dir.path(), &env_with_boot(&world, Some(9_000)));
    assert_eq!(second.safe_mode(), None);
    assert_eq!(meta_on_disk(&dir).crash_count, 1);
    drop(second);

    world.advance(1_000);
    let third = Store::load_with(dir.path(), &env_with_boot(&world, Some(9_000)));
    assert_eq!(third.safe_mode(), Some(SafeModeReason::CrashLoop));
    assert_eq!(meta_on_disk(&dir).crash_count, 2);
    assert_eq!(third.active, config_at(3));
    assert_eq!(
        third.startup_problems()[0].kind,
        ProblemKind::SafeMode {
            reason: SafeModeReason::CrashLoop
        }
    );
    assert!(third.engine.paused.load(Ordering::SeqCst));
    drop(third);

    // An old marker starts over.
    world.advance(5 * 60 * 1000);
    let later = Store::load_with(dir.path(), &env_with_boot(&world, Some(9_000)));
    assert_eq!(later.safe_mode(), None);
    assert_eq!(meta_on_disk(&dir).crash_count, 0);
}

/// S-04 acceptance: a session end (`WM_ENDSESSION` → `RunEvent::Exit`)
/// followed by immediate restarts never reaches SafeMode(CrashLoop).
#[test]
fn session_end_restarts_are_not_crashes() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    for _ in 0..3 {
        let store = Store::load_with(dir.path(), &env(&world));
        assert_eq!(store.safe_mode(), None);
        assert_eq!(meta_on_disk(&dir).crash_count, 0);
        super::register_session_meta(&store);
        super::clear_run_marker_on_session_end();
        assert!(meta_on_disk(&dir).run_marker.is_none());
        world.advance(1_000);
    }
}

/// Plan §7.2: in safe mode every write command returns `SafeMode`; only an
/// import may be committed, and publishing it ends safe mode.
#[test]
fn safe_mode_refuses_writes_and_import_leaves_it() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(2)));
    write(&dir, "revisions/1.json", &bytes(&config_at(1)));
    for _ in 0..2 {
        drop(Store::load_with(dir.path(), &env(&world)));
        world.advance(1_000);
    }
    let mut store = Store::load_with(dir.path(), &env(&world));
    let reason = SafeModeReason::CrashLoop;
    assert_eq!(store.safe_mode(), Some(reason));
    let refused = |result: Result<(), CommandError>| {
        assert!(matches!(result, Err(CommandError::SafeMode { reason: r }) if r == reason));
    };
    let before = files(&dir);
    // save_draft, apply_settings, discard_draft, rollback, LKG marking.
    refused(store.save(metadata_config(&["edited"])));
    refused(store.publish(store.draft.clone()));
    refused(store.discard());
    refused(store.rollback_previous());
    refused(store.mark_last_good(2));
    // The gate of set_paused(false), execute_action and set_autostart.
    refused(store.ensure_writable());
    assert_eq!(files(&dir), before);
    // A draft that is not the previewed import stays refused.
    let imported = metadata_config(&["imported"]);
    store.pending_import = Some((imported.clone(), vec![]));
    refused(store.save(metadata_config(&["other"])));

    store.save(imported.clone()).unwrap();
    assert_eq!(store.safe_mode(), Some(reason));
    refused(store.publish(metadata_config(&["other"])));
    store.publish(store.draft.clone()).unwrap();
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.active.rules, imported.rules);
    assert_eq!(store.active.revision, 3);
    assert!(store
        .startup_problems()
        .iter()
        .all(|p| !matches!(p.kind, ProblemKind::SafeMode { .. })));
    assert!(store.engine.paused.load(Ordering::SeqCst));
    assert_eq!(store.runtime.pause_reasons(), PauseReasons::MANUAL);
    store.ensure_writable().unwrap();
}

/// SafeMode(Locked) and SafeMode(NewerSchema) never replace the unread
/// file, not even with an import.
#[test]
fn read_only_safe_modes_refuse_import_commit() {
    let dir = TempDir::new();
    let world = world();
    let mut newer = serde_json::to_value(config_at(1)).unwrap();
    newer["version"] = json!(SUPPORTED_SCHEMA_VERSION + 1);
    write(&dir, "active.json", &value_bytes(&newer));
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), Some(SafeModeReason::NewerSchema));
    let before = files(&dir);
    let imported = metadata_config(&["imported"]);
    store.pending_import = Some((imported.clone(), vec![]));
    assert!(matches!(
        store.save(imported),
        Err(CommandError::SafeMode {
            reason: SafeModeReason::NewerSchema
        })
    ));
    store.meta.clear_run_marker();
    assert_eq!(files(&dir), before);
}

/// B-12 through the real load path: legacy terminators stay configured and
/// compile; the warning becomes `terminator_unsupported`.
#[test]
fn legacy_terminators_reach_startup_problems() {
    let dir = TempDir::new();
    let world = world();
    let mut config = config_at(1);
    config.globals.terminators = vec!["space".into(), "enter".into(), "tab".into()];
    write(&dir, "active.json", &bytes(&config));
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert!(store.startup_problems().contains(
        &ProblemKind::TerminatorUnsupported {
            keys: vec!["tab".into(), "enter".into()],
        }
        .into()
    ));
    assert_eq!(store.active.globals.terminators, config.globals.terminators);
    assert!(store.engine.compiled.lock().unwrap().rule("a").is_some());
}

/// B-43: rules that do not compile and entries this version cannot read
/// stay off; the count includes entries without an id.
#[test]
fn rules_skipped_counts_unreadable_entries() {
    let dir = TempDir::new();
    let world = world();
    let mut config = metadata_config(&["a", "bad"]);
    config.rules[1].match_spec = MatchSpec {
        kind: MatchKind::Regex,
        value: "(".into(),
    };
    let mut value = serde_json::to_value(&config).unwrap();
    let rules = value["rules"].as_array_mut().unwrap();
    rules.push(json!({"id": "future", "action": {"type": "teleport"}}));
    rules.push(json!({"action": 5}));
    write(&dir, "active.json", &value_bytes(&value));
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert!(kinds(&report).contains(&&ProblemKind::RulesSkipped {
        count: 3,
        ids: vec!["bad".into(), "future".into()],
    }));
    assert!(report.compiled.rule("a").is_some());
    assert!(report.compiled.rule("bad").is_none());
    assert_eq!(report.active.unreadable_rules.len(), 2);
}

/// The `store-meta.corrupt-*.json` files in `dir`.
fn meta_quarantine(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("store-meta.corrupt-")
        })
        .collect()
}
const CORRUPT_META: &[u8] = b"{\"last_good_revision\": 7, \"future\": [1], \"crash_count\": ";

/// A `store-meta.json` that does not parse is quarantined byte for byte
/// before the default one replaces it.
#[test]
fn unreadable_store_meta_is_reset() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    write(&dir, META_FILE, CORRUPT_META);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert!(report.meta_writable);
    assert!(kinds(&report).contains(&&ProblemKind::StoreMetaReset {}));
    let quarantine = meta_quarantine(&dir);
    assert_eq!(quarantine.len(), 1);
    assert_eq!(fs::read(&quarantine[0]).unwrap(), CORRUPT_META);
    let meta = meta_on_disk(&dir);
    assert_eq!(meta.last_good_revision, None);
    assert!(meta.extra.is_empty());
    assert_eq!(meta.crash_count, 0);
    assert_eq!(meta.run_marker.unwrap().pid, 4242);
}

/// When the corrupt `store-meta.json` cannot be renamed it is never
/// overwritten: the run marker stays in memory and startup still completes.
#[test]
fn unrenamable_store_meta_is_left_untouched() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    write(&dir, META_FILE, CORRUPT_META);
    world.rename_fails.store(true, Ordering::SeqCst);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert!(!report.meta_writable);
    assert!(!kinds(&report).contains(&&ProblemKind::StoreMetaReset {}));
    assert_eq!(report.meta.run_marker.as_ref().unwrap().pid, 4242);
    assert_eq!(fs::read(dir.join(META_FILE)).unwrap(), CORRUPT_META);

    // Through the store: neither the run marker nor LKG marking writes it.
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.active, config_at(1));
    store.publish(metadata_config(&["b"])).unwrap();
    store.mark_last_good(store.active.revision).unwrap();
    store.meta.clear_run_marker();
    assert_eq!(fs::read(dir.join(META_FILE)).unwrap(), CORRUPT_META);
    assert!(meta_quarantine(&dir).is_empty());
}

/// The read-only safe modes do not even quarantine a corrupt
/// `store-meta.json`.
#[test]
fn read_only_safe_modes_leave_corrupt_store_meta_alone() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    write(&dir, META_FILE, CORRUPT_META);
    world.locked.lock().push("active.json");
    let before = files(&dir);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(
        report.outcome,
        LoadOutcome::SafeMode(SafeModeReason::Locked)
    );
    assert!(!report.meta_writable);
    assert_eq!(files(&dir), before);
}

/// LKG: the recorded revision is the first fallback candidate.
#[test]
fn mark_last_good_is_the_first_fallback() {
    let dir = TempDir::new();
    let world = world();
    let mut store = Store::load_with(dir.path(), &env(&world));
    store.publish(metadata_config(&["one"])).unwrap();
    store.publish(metadata_config(&["two"])).unwrap();
    store.mark_last_good(2).unwrap();
    assert!(dir.join("revisions/2.json").exists());
    assert_eq!(store.meta.get().last_good_revision, Some(2));
    store.publish(metadata_config(&["three"])).unwrap();
    drop(store);
    write(&dir, "active.json", TRUNCATED);
    world.advance(10 * 60 * 1000);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(
        report.outcome,
        LoadOutcome::Recovered {
            from: RecoverySource::LastGood,
            revision: Some(2)
        }
    );
    assert_eq!(report.active.rules[0].id, "two");
}

const ALL_MODES: [Option<SafeModeReason>; 5] = [
    None,
    Some(SafeModeReason::Locked),
    Some(SafeModeReason::NewerSchema),
    Some(SafeModeReason::NoValidConfig),
    Some(SafeModeReason::CrashLoop),
];

/// A data directory with every store file, loaded in `mode`.
fn store_in(mode: Option<SafeModeReason>) -> (TempDir, Arc<World>, Store) {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(2)));
    write(&dir, "draft.json", &bytes(&config_at(2)));
    write(&dir, "previous.json", &bytes(&config_at(1)));
    write(&dir, "revisions/1.json", &bytes(&config_at(1)));
    atomic_json(&dir.join(META_FILE), &StoreMeta::default()).unwrap();
    match mode {
        None => {}
        Some(SafeModeReason::Locked) => world.locked.lock().push("active.json"),
        Some(SafeModeReason::NewerSchema) => {
            let mut newer = serde_json::to_value(config_at(2)).unwrap();
            newer["version"] = json!(SUPPORTED_SCHEMA_VERSION + 1);
            write(&dir, "active.json", &value_bytes(&newer));
        }
        Some(SafeModeReason::NoValidConfig) => {
            for name in ["active.json", "previous.json", "revisions/1.json"] {
                write(&dir, name, TRUNCATED);
            }
        }
        Some(SafeModeReason::CrashLoop) => {
            for _ in 0..2 {
                drop(Store::load_with(dir.path(), &env(&world)));
                world.advance(1_000);
            }
        }
    }
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), mode);
    (dir, world, store)
}

fn refused_as_data_dir(result: Result<(), CommandError>) -> bool {
    matches!(
        result,
        Err(CommandError::Io {
            op: crate::error::IoOp::Write,
            os_code: None,
            ..
        })
    )
}

/// `export_config` never writes into the data directory: in normal mode and
/// CrashLoop the target is refused as inside it, in the other safe modes
/// the export itself is refused.
#[test]
fn export_never_writes_into_the_data_dir() {
    for mode in ALL_MODES {
        let (dir, _world, store) = store_in(mode);
        let before = files(&dir);
        for name in [
            "active.json",
            "draft.json",
            "previous.json",
            "revisions/1.json",
            META_FILE,
        ] {
            let result = store.export_draft(&dir.join(name));
            match mode {
                None | Some(SafeModeReason::CrashLoop) => {
                    let label = Path::new(name).file_name().unwrap().to_string_lossy();
                    assert!(
                        matches!(
                            &result,
                            Err(CommandError::Io {
                                op: crate::error::IoOp::Write,
                                path: Some(path),
                                os_code: None,
                            }) if *path == label
                        ),
                        "{mode:?} {name}: {result:?}"
                    );
                }
                Some(reason) => assert!(
                    matches!(result, Err(CommandError::SafeMode { reason: r }) if r == reason),
                    "{mode:?} {name}"
                ),
            }
        }
        assert_eq!(files(&dir), before, "{mode:?}");
    }
}

/// CrashLoop and normal mode export outside the data directory, including
/// to a sibling whose name only starts like it.
#[test]
fn crash_loop_exports_outside_the_data_dir() {
    for mode in [None, Some(SafeModeReason::CrashLoop)] {
        let (dir, _world, store) = store_in(mode);
        let elsewhere = TempDir::new();
        let sibling = TempDir(PathBuf::from(format!("{}-backup", dir.display())));
        fs::create_dir_all(&sibling.0).unwrap();
        for target in [elsewhere.join("backup.json"), sibling.join("backup.json")] {
            store.export_draft(&target).unwrap();
            let package = fs::read_to_string(&target).unwrap();
            let imported = import_package(&package).unwrap();
            assert_eq!(imported.config.rules, store.draft.rules, "{mode:?}");
        }
    }
}

/// Other spellings of a path inside the data directory are refused too.
#[test]
fn export_refuses_other_spellings_of_the_data_dir() {
    let (dir, _world, store) = store_in(None);
    let elsewhere = TempDir::new();
    let before = files(&dir);
    let mut variants = vec![
        dir.join("revisions").join("..").join("active.json"),
        elsewhere
            .join("..")
            .join(dir.file_name().unwrap())
            .join("draft.json"),
        dir.join("revisions").join(".."),
        dir.path(),
        dir.join("new-export.json"),
        dir.join("revisions").join("9.json"),
    ];
    if cfg!(windows) {
        let text = dir.display().to_string();
        variants.extend([
            PathBuf::from(text.to_uppercase()).join("ACTIVE.JSON"),
            PathBuf::from(text.to_lowercase())
                .join("Revisions")
                .join("1.JSON"),
            dir.join("active.json."),
            dir.join("draft.json . "),
            dir.join("brand-new.json."),
            PathBuf::from(format!("{text}.")).join("previous.json"),
            PathBuf::from(format!(r"\\?\{text}")).join("active.json"),
            dir.join("active.json::$DATA"),
        ]);
    }
    for target in variants {
        let result = store.export_draft(&target);
        assert!(refused_as_data_dir(result), "{}", target.display());
    }
    assert_eq!(files(&dir), before);
}

/// Replaces the staging steps of `export_config` ([`StageHooks`]).
#[derive(Default)]
struct Seams {
    /// Reports the temp file as created in this directory, as a parent
    /// junction redirected after the pathname check would.
    lands_in: Option<PathBuf>,
    /// Runs between the containment check and the write.
    after_check: Option<Box<dyn FnMut()>>,
    fail_write: bool,
    checked: u32,
}
impl StageHooks for Seams {
    fn final_path(&mut self, file: &fs::File, temp: &Path) -> io::Result<PathBuf> {
        match &self.lands_in {
            Some(dir) => Ok(dir.join(temp.file_name().unwrap())),
            None => final_path(file, temp),
        }
    }
    fn after_check(&mut self, temp: &Path) {
        self.checked += 1;
        assert!(temp.exists());
        if let Some(hook) = &mut self.after_check {
            hook();
        }
    }
    fn write(&mut self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
        if self.fail_write {
            // Part of the package reaches the temp file before the failure.
            io::Write::write_all(file, &bytes[..bytes.len() / 2])?;
            return Err(io::Error::other("injected export write failure"));
        }
        io::Write::write_all(file, bytes)
    }
}

/// Temp files of the staged export left in `dir`.
fn export_temps(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".qt-export-"))
        .collect()
}

/// Makes `link` another name for `original`.
fn alias(kind: &str, original: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as symlink;
    let _ = fs::remove_file(link);
    if kind == "hard_link" {
        fs::hard_link(original, link)
    } else {
        symlink(original, link)
    }
}

/// A temp file that turns out to be inside the data directory (a parent
/// junction redirected after the pathname check) is refused and removed
/// before anything is written; the store and the old target keep their
/// bytes.
#[test]
fn export_refuses_a_temp_file_that_lands_in_the_data_dir() {
    for mode in [None, Some(SafeModeReason::CrashLoop)] {
        let (dir, _world, store) = store_in(mode);
        let elsewhere = TempDir::new();
        let target = elsewhere.join("backup.json");
        fs::write(&target, b"old backup").unwrap();
        let before = files(&dir);
        let mut seams = Seams {
            lands_in: Some(dir.join("revisions")),
            ..Seams::default()
        };
        let result = store.export_draft_with(&target, &mut seams);
        assert!(refused_as_data_dir(result), "{mode:?}");
        assert_eq!(seams.checked, 0, "{mode:?}: nothing written");
        assert_eq!(export_temps(&elsewhere), Vec::<String>::new());
        assert_eq!(fs::read(&target).unwrap(), b"old backup");
        assert_eq!(files(&dir), before, "{mode:?}");
    }
}

/// A target that is, or becomes after the checks, a hard link or symlink to
/// `active.json` is replaced by the export, not written through: the store
/// files keep their bytes and the target ends up a plain file.
#[test]
fn export_replaces_a_swapped_target_instead_of_writing_through_it() {
    let (dir, _world, store) = store_in(None);
    let elsewhere = TempDir::new();
    let active = dir.join("active.json");
    let before = files(&dir);
    let mut kinds = vec!["hard_link"];
    if alias("symlink", &active, &elsewhere.join("probe")).is_ok() {
        kinds.push("symlink");
    } else {
        eprintln!("symlinks unavailable here (privilege); symlink swap not exercised");
    }
    let _ = fs::remove_file(elsewhere.join("probe"));
    for kind in kinds {
        for swap_after_check in [false, true] {
            let case = format!("{kind} swapped after check: {swap_after_check}");
            let target = elsewhere.join(format!("{kind}-{swap_after_check}.json"));
            fs::write(&target, b"old backup").unwrap();
            let mut seams = Seams::default();
            if swap_after_check {
                let (active, target) = (active.clone(), target.clone());
                seams.after_check = Some(Box::new(move || alias(kind, &active, &target).unwrap()));
            } else {
                alias(kind, &active, &target).unwrap();
            }
            let result = store.export_draft_with(&target, &mut seams);
            if kind == "symlink" && !swap_after_check {
                // The pathname check already resolves the link.
                assert!(refused_as_data_dir(result), "{case}");
            } else {
                result.unwrap();
                assert!(!fs::symlink_metadata(&target)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                let package = fs::read_to_string(&target).unwrap();
                let imported = import_package(&package).unwrap();
                assert_eq!(imported.config.rules, store.draft.rules, "{case}");
            }
            assert_eq!(files(&dir), before, "{case}");
            assert_eq!(export_temps(&elsewhere), Vec::<String>::new(), "{case}");
        }
    }
}

/// A failed write or rename removes the temp file and leaves the old target
/// as it was.
#[test]
fn failed_export_leaves_the_target_and_no_temp_file() {
    let (dir, _world, store) = store_in(None);
    let elsewhere = TempDir::new();
    let before = files(&dir);
    let target = elsewhere.join("backup.json");
    fs::write(&target, b"old backup").unwrap();
    let mut seams = Seams {
        fail_write: true,
        ..Seams::default()
    };
    let result = store.export_draft_with(&target, &mut seams);
    assert!(
        matches!(
            result,
            Err(CommandError::Io {
                op: crate::error::IoOp::Write,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(seams.checked, 1);
    assert_eq!(fs::read(&target).unwrap(), b"old backup");
    assert_eq!(export_temps(&elsewhere), Vec::<String>::new());
    // The rename fails when the target is a directory.
    let folder = elsewhere.join("folder.json");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("keep.txt"), b"keep").unwrap();
    let result = store.export_draft(&folder);
    assert!(
        matches!(
            result,
            Err(CommandError::Io {
                op: crate::error::IoOp::Rename,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(folder.join("keep.txt")).unwrap(), b"keep");
    assert_eq!(export_temps(&elsewhere), Vec::<String>::new());
    assert_eq!(files(&dir), before);
}

/// In-memory login item shared with the deferred repair; the real one is
/// never touched.
struct FakeLoginItem {
    entry: parking_lot::Mutex<Option<String>>,
    writes: AtomicU32,
}
impl FakeLoginItem {
    fn stale() -> Arc<Self> {
        Arc::new(Self {
            entry: parking_lot::Mutex::new(Some("\"D:/old/quicker-text.exe\" --background".into())),
            writes: AtomicU32::new(0),
        })
    }
    fn writes(&self) -> u32 {
        self.writes.load(Ordering::SeqCst)
    }
}
impl LoginItem for Arc<FakeLoginItem> {
    fn entry(&self) -> io::Result<Option<String>> {
        Ok(self.entry.lock().clone())
    }
    fn set_entry(&self, entry: &str) -> io::Result<()> {
        *self.entry.lock() = Some(entry.into());
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// B-04: a stale login item is not rewritten during any safe mode; it is
/// repaired exactly once when the store becomes writable.
#[test]
fn autostart_repair_waits_until_the_store_is_writable() {
    let exe = PathBuf::from("C:/Apps/quicker-text.exe");
    let repaired = |store: &Store| {
        store
            .startup_problems()
            .iter()
            .filter(|p| p.kind == ProblemKind::AutostartRepaired {})
            .count()
    };
    for mode in ALL_MODES {
        let (dir, world, mut store) = store_in(mode);
        let item = FakeLoginItem::stale();
        defer_autostart_repair(&mut store, item.clone(), exe.clone());
        // Registering again changes nothing.
        defer_autostart_repair(&mut store, item.clone(), exe.clone());
        match mode {
            None => {}
            Some(SafeModeReason::Locked) => {
                assert_eq!(item.writes(), 0, "{mode:?}");
                world.locked.lock().clear();
                store.apply_retry(retry_locked(&dir, &env(&world)).unwrap());
            }
            Some(SafeModeReason::NewerSchema) => {
                assert_eq!(item.writes(), 0, "{mode:?}");
                // No way out within the session; a reload that finds a
                // supported file stands in for becoming writable.
                write(&dir, "active.json", &bytes(&config_at(2)));
                store.apply_retry(load_or_fallback(&dir, &env(&world)));
            }
            Some(_) => {
                assert_eq!(item.writes(), 0, "{mode:?}");
                let imported = metadata_config(&["imported"]);
                store.pending_import = Some((imported.clone(), vec![]));
                store.save(imported).unwrap();
                assert_eq!(item.writes(), 0, "{mode:?}: committed, not published");
                store.publish(store.draft.clone()).unwrap();
            }
        }
        assert_eq!(store.safe_mode(), None, "{mode:?}");
        assert_eq!(item.writes(), 1, "{mode:?}");
        assert!(item
            .entry
            .lock()
            .as_ref()
            .unwrap()
            .contains("C:/Apps/quicker-text.exe"));
        assert_eq!(repaired(&store), 1, "{mode:?}");

        // Later writes and registrations do not repair again.
        *item.entry.lock() = Some("\"D:/old/quicker-text.exe\" --background".into());
        store.publish(metadata_config(&["later"])).unwrap();
        defer_autostart_repair(&mut store, item.clone(), exe.clone());
        assert_eq!(item.writes(), 1, "{mode:?}");
        assert_eq!(repaired(&store), 1, "{mode:?}");
    }
}

/// Problems found after the load reach the problem sink, one
/// `startup-problem` event each (contract §3): a reload (the Locked retry)
/// reports only what was not listed before, `add_problem` what it adds.
/// What the load itself found is read with `startup_problems`.
#[test]
fn later_problems_are_reported_once() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(2)));
    write(&dir, "draft.json", &bytes(&config_at(2)));
    world.locked.lock().push("active.json");
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), Some(SafeModeReason::Locked));
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    store.set_problem_sink(Arc::new(move |problem: crate::startup::StartupProblem| {
        sink.lock().push(problem.kind);
    }));
    assert!(
        seen.lock().is_empty(),
        "the load's own problems are not resent"
    );

    world.locked.lock().clear();
    let skipped = ProblemKind::RulesSkipped {
        count: 1,
        ids: vec!["x".into()],
    };
    let mut report = retry_locked(&dir, &env(&world)).unwrap();
    report.problems.push(skipped.clone().into());
    store.apply_retry(report);
    assert_eq!(store.safe_mode(), None);
    assert_eq!(seen.lock().as_slice(), std::slice::from_ref(&skipped));
    // Found again by a later reload: already listed, not sent again.
    let mut report = load_or_fallback(&dir, &env(&world));
    report.problems.push(skipped.clone().into());
    store.apply_retry(report);
    assert_eq!(seen.lock().as_slice(), std::slice::from_ref(&skipped));

    store.add_problem(ProblemKind::AutostartRepaired {});
    assert_eq!(*seen.lock(), [skipped, ProblemKind::AutostartRepaired {}]);
}

/// What was found outside the load (autostart, the startup self-test, a
/// restored log level) is still listed after a Locked retry reloads; what
/// the earlier load found is replaced by the reload's own findings.
#[test]
fn problems_found_outside_the_load_survive_a_reload() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(2)));
    write(&dir, "draft.json", &bytes(&config_at(2)));
    world.locked.lock().push("active.json");
    let mut store = Store::load_with(dir.path(), &env(&world));
    let outside = [
        ProblemKind::AutostartRepaired {},
        ProblemKind::SelfTestFailed {
            checks: vec![quicker_platform::self_test::CheckName::Foreground],
        },
        ProblemKind::LogLevelElevated {
            level: crate::diagnostics::level::LogLevel::Debug,
            until_ms: 7,
        },
    ];
    for kind in outside.clone() {
        store.add_problem(kind);
    }
    world.locked.lock().clear();
    store.apply_retry(retry_locked(&dir, &env(&world)).unwrap());
    assert_eq!(store.safe_mode(), None);
    let kinds: Vec<_> = store
        .startup_problems()
        .iter()
        .map(|p| p.kind.clone())
        .collect();
    assert_eq!(kinds, outside);
}

/// Review A4 (contract §3 `store-state`): the frontend takes a snapshot in
/// SafeMode(Locked), then the retry succeeds. The new revision and the end
/// of safe mode arrive together as one `store-state` above the snapshot's
/// `seq`, and the next snapshot (the bus's view) has them. A reload that
/// changes neither sends nothing; publishing and rolling back send one each.
#[test]
fn a_locked_retry_after_a_snapshot_arrives_as_store_state() {
    use crate::{
        app_state::Managed,
        commands::engine::runtime_snapshot,
        events::{EventBus, Observer, Sink, STARTUP_PROBLEM, STORE_STATE},
    };
    use quicker_platform::health::{EngineState, HealthSnapshot, HistoryEntry};
    use std::sync::mpsc;

    struct Channel(mpsc::Sender<(&'static str, Value)>);
    impl Sink for Channel {
        fn emit(&self, name: &'static str, payload: Value) {
            let _ = self.0.send((name, payload));
        }
    }
    struct Quiet;
    impl Observer for Quiet {
        fn status(&mut self, _: &HealthSnapshot, _: &EngineState) {}
        fn executed(&mut self, _: &HistoryEntry) {}
    }
    /// The `store-state` events before the next `startup-problem`, a marker
    /// sent after the step under test (engine events are skipped).
    fn store_states(rx: &mpsc::Receiver<(&'static str, Value)>) -> Vec<Value> {
        let mut sent = vec![];
        loop {
            let (name, payload) = rx.recv_timeout(Duration::from_secs(10)).expect("event");
            match name {
                STARTUP_PROBLEM => return sent,
                STORE_STATE => sent.push(payload),
                _ => {}
            }
        }
    }

    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(2)));
    write(&dir, "draft.json", &bytes(&config_at(2)));
    world.locked.lock().push("active.json");
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), Some(SafeModeReason::Locked));
    let runtime = store.runtime.clone();
    let managed = Managed::new(store);
    let (tx, rx) = mpsc::channel();
    let bus = EventBus::start(&runtime, Channel(tx), Box::new(Quiet));
    let problems = bus.problem_sink();
    let marker = || problems(ProblemKind::AutostartRepaired {}.into());
    // As in setup: setting the sink sends the initial value.
    managed.lock().set_store_sink(bus.store_sink());
    marker();
    let initial = store_states(&rx);
    let locked_revision = Config::default().revision;
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0]["revision"], locked_revision);
    assert_eq!(initial[0]["safe_mode"], "locked");

    let snapshot = runtime_snapshot(bus.view(), &managed);
    assert_eq!(snapshot.revision, locked_revision);
    assert_eq!(snapshot.safe_mode, Some(SafeModeReason::Locked));
    assert!(snapshot.seq > initial[0]["seq"].as_u64().unwrap());

    world.locked.lock().clear();
    let report = retry_locked(&dir, &env(&world)).unwrap();
    managed.lock().apply_retry(report);
    marker();
    let recovered = store_states(&rx);
    assert_eq!(recovered.len(), 1, "revision and safe mode in one event");
    let seq = recovered[0]["seq"].as_u64().unwrap();
    assert!(seq > snapshot.seq);
    assert_eq!(recovered[0]["revision"], 2);
    assert_eq!(recovered[0]["safe_mode"], Value::Null);
    let after = runtime_snapshot(bus.view(), &managed);
    assert!(after.seq > seq);
    assert_eq!((after.revision, after.safe_mode), (2, None));

    // Unchanged: reloading the same files sends nothing.
    managed
        .lock()
        .apply_retry(load_or_fallback(&dir, &env(&world)));
    marker();
    assert_eq!(store_states(&rx), Vec::<Value>::new());

    // Any other cause: a publish, then a rollback (a new revision too).
    {
        let mut store = managed.lock();
        let draft = store.draft.clone();
        store.publish(draft).unwrap();
    }
    marker();
    let published = store_states(&rx);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0]["revision"], 3);
    managed.lock().rollback_previous().unwrap();
    marker();
    let rolled_back = store_states(&rx);
    assert_eq!(rolled_back.len(), 1);
    assert_eq!(rolled_back[0]["revision"], 4);
    assert_eq!(rolled_back[0]["safe_mode"], Value::Null);
}

/// Fault injection (B-52): the store points behave as the real failures.
#[cfg(feature = "fault-injection")]
mod injected {
    use super::*;
    use crate::faults::scope::FaultScope;
    use quicker_platform::fault::{debug_inject_fault, disarm, FaultPoint};

    #[test]
    fn config_corrupt_recovers_from_a_revision() {
        let _scope = FaultScope::new();
        let dir = TempDir::new();
        let mut store = Store::load(dir.path());
        store.publish(metadata_config(&["one"])).unwrap();
        store.publish(metadata_config(&["two"])).unwrap();
        drop(store);
        debug_inject_fault(FaultPoint::ConfigCorrupt, 0);
        let store = Store::load(dir.path());
        assert_eq!(store.safe_mode(), None);
        assert_eq!(store.active.revision, 1);
        assert_eq!(store.active.rules[0].id, "one");
        assert!(store.startup_problems().iter().any(|p| matches!(
            p.kind,
            ProblemKind::Recovered {
                from: RecoverySource::Revision,
                revision: Some(1)
            }
        )));
    }

    #[test]
    fn store_locked_is_read_only_until_disarmed() {
        let _scope = FaultScope::new();
        let dir = TempDir::new();
        let world = world();
        write(&dir, "active.json", &bytes(&config_at(2)));
        write(&dir, "revisions/1.json", &bytes(&config_at(1)));
        let env = StoreEnv {
            io: Box::new(super::super::fallback::FsIo),
            clock: Box::new(FakeClock(world.clone())),
            boot_id: None,
            pid: 1,
        };
        debug_inject_fault(FaultPoint::StoreLocked, 0);
        let before = files(&dir);
        let report = load_or_fallback(&dir, &env);
        assert_eq!(
            report.outcome,
            LoadOutcome::SafeMode(SafeModeReason::Locked)
        );
        // `peek`: still locked on the next attempt.
        assert!(retry_locked(&dir, &env).is_none());
        assert_eq!(files(&dir), before);
        disarm(FaultPoint::StoreLocked);
        let report = retry_locked(&dir, &env).unwrap();
        assert_eq!(report.outcome, LoadOutcome::Normal);
        assert_eq!(report.active, config_at(2));
    }

    /// No-regression #8 under the fault: the failed publish keeps the
    /// active configuration and the pause state.
    #[test]
    fn store_write_fail_keeps_active_and_pause() {
        let _scope = FaultScope::new();
        let dir = TempDir::new();
        let mut store = Store::load(dir.path());
        store.publish(metadata_config(&["one"])).unwrap();
        let old = store.active.clone();
        let on_disk = fs::read(dir.join("active.json")).unwrap();
        listening(&store);
        assert!(!store.engine.paused.load(Ordering::SeqCst));
        debug_inject_fault(FaultPoint::StoreWriteFail, 0);
        assert!(matches!(
            store.publish(metadata_config(&["two"])),
            Err(CommandError::Io {
                op: crate::error::IoOp::Rename,
                ..
            })
        ));
        assert_eq!(store.active, old);
        assert_eq!(fs::read(dir.join("active.json")).unwrap(), on_disk);
        assert!(!store.engine.paused.load(Ordering::SeqCst));
        assert!(store.runtime.engine_state().listening);
        // One-shot: the next publish goes through.
        store.publish(metadata_config(&["two"])).unwrap();
        assert_eq!(store.active.revision, 2);
    }
}

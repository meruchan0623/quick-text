//! Startup self-healing and safe mode (S-04 / B-04 acceptance, plan §7.2).
//! Every test works in its own temporary directory with a fake clock and,
//! where locks or failed renames are needed, fake file access.
use super::{
    atomic::atomic_json,
    fallback::{load_or_fallback, retry_locked, LoadOutcome, LoadReport, StoreEnv, StoreIo},
    meta::{parse_meta, StoreMeta, META_FILE},
    test_support::*,
    RecoverySource, Store,
};
use crate::{
    error::{CommandError, SafeModeReason},
    startup::ProblemKind,
};
use quicker_domain::*;
use quicker_platform::traits::Clock;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
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
    store.ensure_writable().unwrap();
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

#[test]
fn unreadable_store_meta_is_reset() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    write(&dir, META_FILE, b"{\"crash_count\": ");
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert!(kinds(&report).contains(&&ProblemKind::StoreMetaReset {}));
    let meta = meta_on_disk(&dir);
    assert_eq!(meta.crash_count, 0);
    assert_eq!(meta.run_marker.unwrap().pid, 4242);
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
        store.engine.paused.store(false, Ordering::SeqCst);
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
        // One-shot: the next publish goes through.
        store.publish(metadata_config(&["two"])).unwrap();
        assert_eq!(store.active.revision, 2);
    }
}

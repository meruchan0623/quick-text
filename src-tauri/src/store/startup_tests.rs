//! Startup self-healing and safe mode (S-04 / B-04 acceptance, plan §7.2).
//! Every test works in its own temporary directory with a fake clock and,
//! where locks or failed renames are needed, fake file access.
use super::{
    atomic::atomic_json,
    export::{final_path, StageHooks},
    fallback::{
        load_or_fallback, retry_locked, revision_numbers, LoadOutcome, LoadReport, StoreEnv,
        StoreIo,
    },
    load::read_config,
    meta::{parse_meta, Fingerprint, PreviousRevision, StoreMeta, META_FILE},
    test_support::*,
    RecoverySource, Store,
};
use crate::{
    autostart::LoginItem,
    error::{CommandError, EntityKind, SafeModeReason},
    startup::{defer_autostart_repair, end_session, finish_self_test, ProblemKind, StartupListen},
};
use quicker_domain::*;
use quicker_platform::{
    health::PauseReasons,
    self_test::{CheckName, SelfTestCheck, SelfTestReport},
    traits::Clock,
};
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
    /// Directories whose listing fails, with the OS error code it fails with.
    unlisted: parking_lot::Mutex<Vec<(PathBuf, i32)>>,
}
fn world() -> Arc<World> {
    Arc::new(World {
        wall_ms: AtomicI64::new(1_800_000_000_000),
        sleeps: parking_lot::Mutex::new(vec![]),
        locked: parking_lot::Mutex::new(vec![]),
        rename_fails: AtomicBool::new(false),
        unlisted: parking_lot::Mutex::new(vec![]),
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
    fn list_dir(&self, dir: &Path) -> io::Result<Vec<std::ffi::OsString>> {
        let unlisted = self.0.unlisted.lock();
        if let Some((_, code)) = unlisted.iter().find(|(path, _)| path == dir) {
            return Err(io::Error::from_raw_os_error(*code));
        }
        fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
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
/// Marks the pre-0.4 backup as dealt with in store-meta.json, as a launch of
/// 0.4 that made it leaves the directory: the load then renames corrupt
/// files aside itself instead of leaving them for the backup (review I-B1).
fn settled(dir: &Path) {
    let meta = StoreMeta {
        backup_pre_0_4: Some(true),
        ..StoreMeta::default()
    };
    atomic_json(&dir.join(META_FILE), &meta).unwrap();
}

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
        if matches!(name, "truncated" | "invalid utf-8") {
            // Renamed aside at the load, as after the pre-0.4 backup.
            settled(&dir);
        }
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
    settled(&dir);
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
    settled(&dir);
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

/// Ends a session the way a normal exit does: the run marker is cleared.
fn clean_exit(store: Store, world: &World) {
    store.meta.clear_run_marker();
    drop(store);
    world.advance(1_000);
}
fn assert_no_valid_config(store: &Store) {
    let reason = SafeModeReason::NoValidConfig;
    assert_eq!(store.safe_mode(), Some(reason));
    assert_eq!(
        store.startup_problems()[0].kind,
        ProblemKind::SafeMode { reason }
    );
    assert_eq!(store.active, Config::default());
    assert!(store.engine.paused.load(Ordering::SeqCst));
}
/// The quarantined files in `dir` with their content.
fn quarantine_files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    files(dir)
        .into_iter()
        .filter(|(path, _)| path.to_string_lossy().contains(".corrupt-"))
        .collect()
}

/// F4-1: SafeMode(NoValidConfig) survives clean restarts. After the first
/// launch the quarantined candidates are all that is left, so later ones
/// find no candidate file; they must not take the profile for a new data
/// directory. This is read from the directory, so it holds even when
/// `store-meta.json` cannot be read.
#[test]
fn no_valid_config_survives_clean_restarts() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "revisions/1.json", b"\xFF");
    write(&dir, "previous.json", b"[]");
    let before = files(&dir);
    settled(&dir);
    let store = Store::load_with(dir.path(), &env(&world));
    assert_no_valid_config(&store);
    let problems: Vec<_> = store.startup_problems().iter().map(|p| &p.kind).collect();
    assert_eq!(quarantined(&problems).len(), 3);
    clean_exit(store, &world);
    let quarantine = quarantine_files(&dir);
    assert_eq!(quarantine.len(), 3);

    for _ in 0..2 {
        let store = Store::load_with(dir.path(), &env(&world));
        assert_no_valid_config(&store);
        let problems: Vec<_> = store.startup_problems().iter().map(|p| &p.kind).collect();
        assert!(quarantined(&problems).is_empty());
        assert_eq!(quarantine_files(&dir), quarantine);
        assert!(!dir.join("active.json").exists());
        clean_exit(store, &world);
    }

    world.locked.lock().push(META_FILE);
    let report = load_or_fallback(&dir, &env(&world));
    let reason = SafeModeReason::NoValidConfig;
    assert_eq!(report.outcome, LoadOutcome::SafeMode(reason));
    assert!(!report.meta_writable);
    assert_eq!(kinds(&report)[0], &ProblemKind::SafeMode { reason });
    assert_nothing_lost(&before, &files(&dir));
}

/// F4-1: a draft that outlived every configuration stays protected on the
/// launches after the first: it is loaded as it is, and every write that
/// could replace it is refused with the safe mode.
#[test]
fn no_valid_config_keeps_protecting_a_surviving_draft() {
    let dir = TempDir::new();
    let world = world();
    let drafted = metadata_config(&["drafted"]);
    for name in ["active.json", "previous.json", "revisions/1.json"] {
        write(&dir, name, TRUNCATED);
    }
    write(&dir, "draft.json", &bytes(&drafted));
    settled(&dir);
    let draft_bytes = fs::read(dir.join("draft.json")).unwrap();
    let store = Store::load_with(dir.path(), &env(&world));
    assert_no_valid_config(&store);
    assert_eq!(store.draft, drafted);
    clean_exit(store, &world);

    let refused = |result: Result<(), CommandError>| {
        assert!(
            matches!(
                result,
                Err(CommandError::SafeMode {
                    reason: SafeModeReason::NoValidConfig
                })
            ),
            "{result:?}"
        );
    };
    for _ in 0..2 {
        let mut store = Store::load_with(dir.path(), &env(&world));
        assert_no_valid_config(&store);
        assert_eq!(store.draft, drafted);
        refused(store.discard());
        refused(store.save(metadata_config(&["edited"])));
        refused(store.publish(store.draft.clone()));
        refused(store.rollback_previous());
        refused(store.mark_last_good(1));
        refused(store.ensure_writable());
        refused(store.set_manual_pause(false, || true));
        assert_eq!(fs::read(dir.join("draft.json")).unwrap(), draft_bytes);
        assert!(!dir.join("active.json").exists());
        clean_exit(store, &world);
    }
}

/// F4-1: publishing an import, also on a later launch, is the way out of
/// SafeMode(NoValidConfig). It writes `active.json`, so the launches after
/// it are Normal. An empty import starts from a blank configuration.
#[test]
fn published_import_ends_no_valid_config_for_good() {
    for imported in [metadata_config(&["imported"]), Config::default()] {
        let dir = TempDir::new();
        let world = world();
        write(&dir, "active.json", TRUNCATED);
        write(&dir, "revisions/1.json", TRUNCATED);
        let store = Store::load_with(dir.path(), &env(&world));
        assert_no_valid_config(&store);
        clean_exit(store, &world);

        let mut store = Store::load_with(dir.path(), &env(&world));
        assert_no_valid_config(&store);
        store.pending_import = Some((imported.clone(), vec![]));
        store.save(imported.clone()).unwrap();
        store.publish(store.draft.clone()).unwrap();
        assert_eq!(store.safe_mode(), None);
        assert!(dir.join("active.json").exists());
        clean_exit(store, &world);

        for _ in 0..2 {
            let store = Store::load_with(dir.path(), &env(&world));
            assert_eq!(store.safe_mode(), None);
            assert_eq!(store.active.rules, imported.rules);
            assert!(store
                .startup_problems()
                .iter()
                .all(|p| !matches!(p.kind, ProblemKind::SafeMode { .. })));
            clean_exit(store, &world);
        }
        assert_eq!(
            load_or_fallback(&dir, &env(&world)).outcome,
            LoadOutcome::Normal
        );
    }
}

/// F4-1: a new data directory is Normal, also next to quarantines that are
/// not configuration candidates (a draft, `store-meta.json`, prefs).
#[test]
fn new_data_directory_is_normal() {
    let dir = TempDir::new();
    let world = world();
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert!(report.problems.is_empty());
    for name in ["draft", "store-meta", "prefs"] {
        write(
            &dir,
            &format!("{name}.corrupt-20261001-000000.json"),
            TRUNCATED,
        );
    }
    world.advance(10 * 60 * 1000);
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert!(store.startup_problems().is_empty());
}

fn refused_no_valid_config(result: Result<(), CommandError>) {
    assert!(
        matches!(
            result,
            Err(CommandError::SafeMode {
                reason: SafeModeReason::NoValidConfig
            })
        ),
        "{result:?}"
    );
}

/// F4-1 review R1: a publish out of SafeMode(NoValidConfig) that fails
/// after the backup stage, at the last write before `active.json`, leaves no
/// valid candidate behind. The empty stand-in is not backed up as a
/// revision, so the next launches are still NoValidConfig and the imported
/// draft stays protected. Publishing again once the write works is still
/// the way out.
#[test]
fn interrupted_recovery_publish_stays_no_valid_config() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "revisions/1.json", TRUNCATED);
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_no_valid_config(&store);
    let imported = metadata_config(&["imported"]);
    store.pending_import = Some((imported.clone(), vec![]));
    store.save(imported.clone()).unwrap();
    let blocker = dir.join("active-migration-unresolved.json");
    fs::create_dir(&blocker).unwrap();
    assert!(store.publish(store.draft.clone()).is_err());
    assert_no_valid_config(&store);
    assert!(!dir.join("active.json").exists());
    assert_eq!(revision_numbers(&dir), Vec::<u64>::new());
    clean_exit(store, &world);

    for _ in 0..2 {
        let mut store = Store::load_with(dir.path(), &env(&world));
        assert_no_valid_config(&store);
        assert_eq!(store.draft.rules, imported.rules);
        let draft_bytes = fs::read(dir.join("draft.json")).unwrap();
        refused_no_valid_config(store.discard());
        refused_no_valid_config(store.save(Config::default()));
        refused_no_valid_config(store.rollback_previous());
        refused_no_valid_config(store.mark_last_good(0));
        refused_no_valid_config(store.set_manual_pause(false, || true));
        assert_eq!(fs::read(dir.join("draft.json")).unwrap(), draft_bytes);
        clean_exit(store, &world);
    }

    fs::remove_dir(&blocker).unwrap();
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_no_valid_config(&store);
    store.pending_import = Some((store.draft.clone(), vec![]));
    store.save(store.draft.clone()).unwrap();
    store.publish(store.draft.clone()).unwrap();
    assert_eq!(store.safe_mode(), None);
    clean_exit(store, &world);
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.active.rules, imported.rules);
    assert!(matches!(
        store.rollback_previous(),
        Err(CommandError::NotFound {
            kind: EntityKind::Revision,
            ..
        })
    ));
}

/// F4-1 review R1: publish backs up the active configuration as
/// `revisions/<n>.json` only when it was on disk. The empty stand-in of a
/// new data directory is neither backed up, nor a rollback target, nor
/// snapshotted as last-known-good; every published configuration still is
/// backed up, rolled back to (also after a restart) and snapshotted.
#[test]
fn publish_backs_up_only_configurations_that_were_on_disk() {
    let dir = TempDir::new();
    let world = world();
    let not_found = |result: Result<(), CommandError>| {
        assert!(
            matches!(
                result,
                Err(CommandError::NotFound {
                    kind: EntityKind::Revision,
                    ..
                })
            ),
            "{result:?}"
        );
    };
    let mut store = Store::load_with(dir.path(), &env(&world));
    not_found(store.mark_last_good(0));
    assert_eq!(store.meta.get().last_good_revision, None);
    let one = metadata_config(&["one"]);
    store.publish(one.clone()).unwrap();
    assert_eq!(revision_numbers(&dir), Vec::<u64>::new());
    not_found(store.rollback_previous());
    let two = metadata_config(&["two"]);
    store.publish(two.clone()).unwrap();
    assert_eq!(revision_numbers(&dir), [1]);
    assert_eq!(
        read_config(&dir.join("revisions/1.json")).unwrap().rules,
        one.rules
    );
    clean_exit(store, &world);

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    store.rollback_previous().unwrap();
    assert_eq!(store.active.revision, 3);
    assert_eq!(store.active.rules, one.rules);
    let mut numbers = revision_numbers(&dir);
    numbers.sort_unstable();
    assert_eq!(numbers, [1, 2]);
    store.mark_last_good(3).unwrap();
    assert_eq!(
        read_config(&dir.join("revisions/3.json")).unwrap().rules,
        one.rules
    );
    assert_eq!(store.meta.get().last_good_revision, Some(3));
}

/// OS error codes for a listing that fails: access denied on Windows (EIO
/// elsewhere), and "not found" on both.
const LIST_DENIED: i32 = 5;
const LIST_NOT_FOUND: i32 = 2;

/// F4-1 review R2: when the directories that hold the quarantine evidence
/// cannot be listed and no candidate is usable, the launch fails closed to
/// SafeMode(NoValidConfig), never to a new data directory: the surviving
/// draft stays protected and every write that could replace it is refused.
/// Covered for `revisions` and for the data directory itself.
#[test]
fn unlistable_evidence_keeps_no_valid_config_and_the_draft() {
    assert_ne!(
        io::Error::from_raw_os_error(LIST_DENIED).kind(),
        io::ErrorKind::NotFound
    );
    let drafted = metadata_config(&["drafted"]);
    // The candidate the first launch quarantines, and the directory that
    // later cannot be listed.
    for (candidate, unlisted) in [("revisions/1.json", "revisions"), ("active.json", "")] {
        let dir = TempDir::new();
        let world = world();
        write(&dir, candidate, TRUNCATED);
        write(&dir, "draft.json", &bytes(&drafted));
        settled(&dir);
        let draft_bytes = fs::read(dir.join("draft.json")).unwrap();
        let store = Store::load_with(dir.path(), &env(&world));
        assert_no_valid_config(&store);
        clean_exit(store, &world);
        assert_eq!(quarantine_files(&dir).len(), 1, "{candidate}");

        let unlisted = if unlisted.is_empty() {
            dir.path()
        } else {
            dir.join(unlisted)
        };
        world.unlisted.lock().push((unlisted, LIST_DENIED));
        for _ in 0..2 {
            let mut store = Store::load_with(dir.path(), &env(&world));
            assert_no_valid_config(&store);
            assert_eq!(store.draft, drafted, "{candidate}");
            refused_no_valid_config(store.discard());
            refused_no_valid_config(store.save(Config::default()));
            refused_no_valid_config(store.rollback_previous());
            refused_no_valid_config(store.set_manual_pause(false, || true));
            assert_eq!(fs::read(dir.join("draft.json")).unwrap(), draft_bytes);
            assert!(!dir.join("active.json").exists());
            clean_exit(store, &world);
        }
        // Unlisted, the data directory of a new profile is no exception.
        let dir = TempDir::new();
        world.unlisted.lock().push((dir.path(), LIST_DENIED));
        assert_eq!(
            load_or_fallback(&dir, &env(&world)).outcome,
            LoadOutcome::SafeMode(SafeModeReason::NoValidConfig)
        );
    }
}

/// F4-1 review R2: a listing error does not change the outcome when a
/// candidate is usable: `active.json`, or a fallback found without the
/// listing (last-known-good, `previous.json`).
#[test]
fn listing_errors_do_not_matter_when_a_candidate_is_usable() {
    let deny_all = |world: &World, dir: &Path| {
        let mut unlisted = world.unlisted.lock();
        unlisted.push((dir.to_path_buf(), LIST_DENIED));
        unlisted.push((dir.join("revisions"), LIST_DENIED));
    };
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    deny_all(&world, &dir);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert!(report.problems.is_empty());
    assert_eq!(report.active, config_at(1));

    let dir = TempDir::new();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "revisions/1.json", &bytes(&config_at(1)));
    let meta = StoreMeta {
        last_good_revision: Some(1),
        ..StoreMeta::default()
    };
    atomic_json(&dir.join(META_FILE), &meta).unwrap();
    deny_all(&world, &dir);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(
        report.outcome,
        LoadOutcome::Recovered {
            from: RecoverySource::LastGood,
            revision: Some(1)
        }
    );

    let dir = TempDir::new();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "previous.json", &bytes(&config_at(4)));
    deny_all(&world, &dir);
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

/// F4-1 review R2: a directory that is not found is empty, so a new data
/// directory stays Normal.
#[test]
fn directories_not_found_are_empty() {
    assert_eq!(
        io::Error::from_raw_os_error(LIST_NOT_FOUND).kind(),
        io::ErrorKind::NotFound
    );
    let dir = TempDir::new();
    let world = world();
    world
        .unlisted
        .lock()
        .extend([dir.path(), dir.join("revisions")].map(|d| (d, LIST_NOT_FOUND)));
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert!(report.problems.is_empty());
    world.advance(10 * 60 * 1000);
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert!(store.startup_problems().is_empty());
}

/// Writes `revisions/<revision>.json` with the one rule `id`, and its
/// metadata: a migration original marked `id`.
fn write_revision(dir: &Path, revision: u64, id: &str) -> Config {
    let config = Config {
        revision,
        ..metadata_config(&[id])
    };
    write(dir, &format!("revisions/{revision}.json"), &bytes(&config));
    let metadata = dir.join(format!("revisions/{revision}.metadata.json"));
    atomic_json(&metadata, &vec![original(id, id)]).unwrap();
    config
}
/// The first record in store-meta.json, the one of the active revision:
/// its active revision and predecessor. Its fingerprint matches active.json.
fn active_record(dir: &Path) -> (u64, Option<u64>) {
    let meta = meta_on_disk(dir);
    let record = meta.previous_revisions.first().expect("a record");
    let active = fs::read(dir.join("active.json")).unwrap();
    assert_eq!(record.fingerprint, Fingerprint::of(&active));
    (record.active, record.revision)
}
/// What rollback would return to, with its migration metadata.
fn rollback_target(store: &Store) -> (Option<Config>, Value) {
    let metadata = serde_json::to_value(&store.previous_unresolved).unwrap();
    (store.previous.clone(), metadata)
}

/// F4-1 review R3: rollback returns to the same configuration before and
/// after a restart. Recovered from last-known-good revision 1 (bypassing
/// revision 2), revision 3 is published over revision 1: after a clean
/// restart rollback restores revision 1 with its migration metadata, not
/// revision 2. A rollback is a publish too: its predecessor survives the
/// next restart as well, and rolling back again goes back again.
#[test]
fn rollback_target_survives_a_restart_after_recovery() {
    let dir = TempDir::new();
    let world = world();
    let one = write_revision(&dir, 1, "one");
    write_revision(&dir, 2, "two");
    write(&dir, "active.json", TRUNCATED);
    let meta = StoreMeta {
        last_good_revision: Some(1),
        ..StoreMeta::default()
    };
    atomic_json(&dir.join(META_FILE), &meta).unwrap();
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active, one);
    let three = metadata_config(&["three"]);
    store.publish(three.clone()).unwrap();
    assert_eq!(store.active.revision, 3);
    assert_eq!(store.previous.as_ref().unwrap().rules, one.rules);
    assert_eq!(active_record(&dir), (3, Some(1)));
    let target = rollback_target(&store);
    clean_exit(store, &world);

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.active.revision, 3);
    assert_eq!(rollback_target(&store), target);
    store.rollback_previous().unwrap();
    assert_eq!(store.active.revision, 4);
    assert_eq!(store.active.rules, one.rules);
    let restored = BTreeMap::from([("one".to_string(), "one".to_string())]);
    assert_eq!(metadata_sources(store.unresolved.clone()), restored);
    assert_eq!(store.previous.as_ref().unwrap().rules, three.rules);
    let target = rollback_target(&store);
    clean_exit(store, &world);

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active.rules, one.rules);
    assert_eq!(metadata_sources(store.active_unresolved.clone()), restored);
    assert_eq!(rollback_target(&store), target);
    store.rollback_previous().unwrap();
    assert_eq!(store.active.revision, 5);
    assert_eq!(store.active.rules, three.rules);
    assert_eq!(store.previous.as_ref().unwrap().rules, one.rules);
    let target = rollback_target(&store);
    clean_exit(store, &world);
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(rollback_target(&store), target);
}

/// F4-1 review R3: without a record for the active revision the
/// predecessor is guessed as before (`revisions/<active - 1>.json`, then
/// `previous.json`): no `previous_revisions` in store-meta.json (a file from
/// before it existed), an entry for another active revision (one published
/// by 0.3.1), a recorded snapshot that is gone, or (review I-R2) a record
/// whose fingerprint is not that of the active.json just read. A record
/// that applies wins over the guess, also as the second of two. When store-meta.json is read-only for a session,
/// its publishes leave it untouched and the next launch guesses as before.
#[test]
fn rollback_target_without_a_record_is_guessed_as_before() {
    let world = world();
    let dir = TempDir::new();
    let one = write_revision(&dir, 1, "one");
    let two = write_revision(&dir, 2, "two");
    let three = Config {
        revision: 3,
        ..metadata_config(&["three"])
    };
    write(&dir, "active.json", &bytes(&three));
    let previous_with = |recorded: Vec<PreviousRevision>| {
        let meta = StoreMeta {
            previous_revisions: recorded,
            ..StoreMeta::default()
        };
        atomic_json(&dir.join(META_FILE), &meta).unwrap();
        load_or_fallback(&dir, &env(&world)).previous
    };
    let matching = Fingerprint::of(&bytes(&three));
    let other = Fingerprint::of(&bytes(&Config {
        revision: 3,
        ..metadata_config(&["other"])
    }));
    let record = |active, revision, fingerprint: &Fingerprint| PreviousRevision {
        active,
        revision,
        fingerprint: fingerprint.clone(),
    };
    for guessed in [
        vec![],
        vec![record(2, Some(1), &matching)],
        vec![record(3, Some(7), &matching)],
        vec![record(3, Some(1), &other)],
        vec![record(3, None, &other)],
    ] {
        assert_eq!(
            previous_with(guessed.clone()),
            Some(two.clone()),
            "{guessed:?}"
        );
    }
    assert_eq!(
        previous_with(vec![record(3, Some(1), &matching)]),
        Some(one.clone())
    );
    assert_eq!(
        previous_with(vec![record(3, None, &other), record(3, Some(1), &matching)]),
        Some(one)
    );
    assert_eq!(previous_with(vec![record(3, None, &matching)]), None);

    let dir = TempDir::new();
    write_revision(&dir, 1, "one");
    let two = Config {
        revision: 2,
        ..metadata_config(&["two"])
    };
    write(&dir, "active.json", &bytes(&two));
    write(&dir, META_FILE, b"{}");
    world.locked.lock().push(META_FILE);
    let mut store = Store::load_with(dir.path(), &env(&world));
    store.publish(metadata_config(&["three"])).unwrap();
    assert_eq!(store.previous, Some(two.clone()));
    assert_eq!(fs::read(dir.join(META_FILE)).unwrap(), b"{}");
    clean_exit(store, &world);
    world.locked.lock().clear();
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active.revision, 3);
    assert_eq!(store.previous, Some(two));
}

/// F4-1 review R3: a revision published over the empty stand-in of a new
/// data directory has no rollback target, also after a restart: not even
/// a `previous.json` that the guess would take.
#[test]
fn stand_in_has_no_rollback_target_after_a_restart() {
    let dir = TempDir::new();
    let world = world();
    let mut store = Store::load_with(dir.path(), &env(&world));
    store.publish(metadata_config(&["first"])).unwrap();
    assert_eq!(store.previous, None);
    assert_eq!(active_record(&dir), (1, None));
    clean_exit(store, &world);
    write(&dir, "previous.json", &bytes(&config_at(0)));
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.previous, None);
    assert!(matches!(
        store.rollback_previous(),
        Err(CommandError::NotFound {
            kind: EntityKind::Revision,
            ..
        })
    ));
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
    settled(&dir);
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

/// Before the pre-0.4 backup the corrupt draft stays where it is, listed
/// for the store to rename once the backup holds it (review I-B1).
#[test]
fn corrupt_draft_is_quarantined_and_restarts_from_active() {
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    write(&dir, "draft.json", TRUNCATED);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert_eq!(report.draft, config_at(1));
    assert!(quarantined(&kinds(&report)).is_empty());
    let deferred: Vec<_> = report.deferred.iter().map(|d| &d.path).collect();
    assert_eq!(deferred, [&dir.join("draft.json")]);
    assert_eq!(fs::read(dir.join("draft.json")).unwrap(), TRUNCATED);

    settled(&dir);
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(report.outcome, LoadOutcome::Normal);
    assert_eq!(report.draft, config_at(1));
    assert_eq!(quarantined(&kinds(&report)), ["draft.json"]);
    assert!(report.deferred.is_empty());
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

/// Tests that go through the process-wide session meta
/// (`register_session_meta`) run one at a time.
static SESSION_META_TESTS: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// S-04 acceptance: a session end (`WM_ENDSESSION` → `RunEvent::Exit`)
/// followed by immediate restarts never reaches SafeMode(CrashLoop).
#[test]
fn session_end_restarts_are_not_crashes() {
    let _one_at_a_time = SESSION_META_TESTS.lock();
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

/// R5, plan B-04: the session-end callback (`WM_ENDSESSION`, or the
/// `session_end` fault replaying it on the session window, which never
/// reaches tao) stops the engine and clears the run marker itself. The
/// process may then end without `RunEvent::Exit`; restarting right away,
/// again and again, is not a crash loop.
#[test]
fn session_end_callback_clears_the_marker_without_exit() {
    let _one_at_a_time = SESSION_META_TESTS.lock();
    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", &bytes(&config_at(1)));
    for _ in 0..3 {
        let store = Store::load_with(dir.path(), &env(&world));
        assert_eq!(store.safe_mode(), None);
        assert_eq!(meta_on_disk(&dir).crash_count, 0);
        assert!(meta_on_disk(&dir).run_marker.is_some());
        super::register_session_meta(&store);
        end_session(&store.runtime);
        assert!(store.runtime.shutting_down());
        assert!(meta_on_disk(&dir).run_marker.is_none());
        // The process ends: no RunEvent::Exit, nothing else clears.
        drop(store);
        world.advance(1_000);
    }
    // The same restarts without the callback are a crash loop.
    for _ in 0..2 {
        drop(Store::load_with(dir.path(), &env(&world)));
        world.advance(1_000);
    }
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), Some(SafeModeReason::CrashLoop));
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

/// Records what the EventBus emits.
struct EventChannel(std::sync::mpsc::Sender<(&'static str, Value)>);
impl crate::events::Sink for EventChannel {
    fn emit(&self, name: &'static str, payload: Value) {
        let _ = self.0.send((name, payload));
    }
}

/// An observer that ignores everything, or (`held`) holds the bus thread
/// in its first callback, which `Worker::run` makes before it takes
/// anything from its queue, until the test releases it.
struct HeldObserver {
    held: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}
impl crate::events::Observer for HeldObserver {
    fn status(
        &mut self,
        _: &quicker_platform::health::HealthSnapshot,
        _: &quicker_platform::health::EngineState,
    ) {
        if let Some((entered, release)) = self.held.take() {
            let _ = entered.send(());
            let _ = release.recv_timeout(Duration::from_secs(10));
        }
    }
    fn executed(&mut self, _: &quicker_platform::health::HistoryEntry) {}
}

/// The `store-state` events before the next `startup-problem`, a marker
/// sent after the step under test (engine events are skipped).
fn store_states(rx: &std::sync::mpsc::Receiver<(&'static str, Value)>) -> Vec<Value> {
    use crate::events::{STARTUP_PROBLEM, STORE_STATE};
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

/// A store in SafeMode(Locked) (active.json locked), managed, with an
/// EventBus started and the store sink set under one store lock, as in
/// setup (the problem sink is left out: `startup-problem` is the marker).
struct LockedBus {
    dir: TempDir,
    world: Arc<World>,
    managed: crate::app_state::Managed,
    bus: crate::events::EventBus,
    rx: std::sync::mpsc::Receiver<(&'static str, Value)>,
}
impl LockedBus {
    fn start(observer: HeldObserver) -> Self {
        let dir = TempDir::new();
        let world = world();
        write(&dir, "active.json", &bytes(&config_at(2)));
        write(&dir, "draft.json", &bytes(&config_at(2)));
        world.locked.lock().push("active.json");
        let store = Store::load_with(dir.path(), &env(&world));
        assert_eq!(store.safe_mode(), Some(SafeModeReason::Locked));
        let runtime = store.runtime.clone();
        let managed = crate::app_state::Managed::new(store);
        let (tx, rx) = std::sync::mpsc::channel();
        let bus = {
            let mut store = managed.lock();
            let bus = crate::events::EventBus::start(
                &runtime,
                store.store_state(),
                EventChannel(tx),
                Box::new(observer),
            );
            store.set_store_sink(bus.store_sink());
            bus
        };
        Self {
            dir,
            world,
            managed,
            bus,
            rx,
        }
    }
    fn marker(&self) {
        (self.bus.problem_sink())(ProblemKind::AutostartRepaired {}.into());
    }
    fn retry_succeeds(&self) {
        self.world.locked.lock().clear();
        let report = retry_locked(&self.dir, &env(&self.world)).unwrap();
        self.managed.lock().apply_retry(report);
    }
    fn snapshot(&self) -> crate::commands::engine::RuntimeSnapshot {
        crate::commands::engine::runtime_snapshot(&self.bus, &self.managed)
    }
}

/// Review A4/A9 (contract §3 `store-state`): setting the sink makes the
/// store's value the bus's baseline (in the snapshot, never sent). The
/// frontend takes a snapshot in SafeMode(Locked), then the retry succeeds:
/// the new revision and the end of safe mode arrive together as one
/// `store-state` above the snapshot's `seq`, and the next snapshot has
/// them. A reload that changes neither sends nothing; publishing and
/// rolling back send one each.
#[test]
fn a_locked_retry_after_a_snapshot_arrives_as_store_state() {
    let t = LockedBus::start(HeldObserver { held: None });
    let locked_revision = Config::default().revision;
    // The baseline: in the snapshot at once, with no event.
    let snapshot = t.snapshot();
    assert_eq!(snapshot.seq, 0);
    assert_eq!(snapshot.revision, locked_revision);
    assert_eq!(snapshot.safe_mode, Some(SafeModeReason::Locked));
    t.marker();
    assert_eq!(
        store_states(&t.rx),
        Vec::<Value>::new(),
        "baseline not sent"
    );

    t.retry_succeeds();
    t.marker();
    let recovered = store_states(&t.rx);
    assert_eq!(recovered.len(), 1, "revision and safe mode in one event");
    let seq = recovered[0]["seq"].as_u64().unwrap();
    assert!(seq > snapshot.seq);
    assert_eq!(recovered[0]["revision"], 2);
    assert_eq!(recovered[0]["safe_mode"], Value::Null);
    let after = t.snapshot();
    assert!(after.seq > seq);
    assert_eq!((after.revision, after.safe_mode), (2, None));

    // Unchanged: reloading the same files sends nothing.
    t.managed
        .lock()
        .apply_retry(load_or_fallback(&t.dir, &env(&t.world)));
    t.marker();
    assert_eq!(store_states(&t.rx), Vec::<Value>::new());

    // Any other cause: a publish, then a rollback (a new revision too).
    {
        let mut store = t.managed.lock();
        let draft = store.draft.clone();
        store.publish(draft).unwrap();
    }
    t.marker();
    let published = store_states(&t.rx);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0]["revision"], 3);
    t.managed.lock().rollback_previous().unwrap();
    t.marker();
    let rolled_back = store_states(&t.rx);
    assert_eq!(rolled_back.len(), 1);
    assert_eq!(rolled_back[0]["revision"], 4);
    assert_eq!(rolled_back[0]["safe_mode"], Value::Null);
}

/// Review A9, the order that used to regress: the bus thread is held
/// before it takes anything from its queue; setting the sink does not wait
/// for it; the Locked retry succeeds and its report is queued; the
/// frontend takes a snapshot; then the bus runs. The snapshot shows the
/// baseline (Locked) at `seq` 0, and no event after it moves the revision
/// back or brings safe mode back: the one `store-state` is the recovery.
/// (Before A9 the initial value was queued too and the snapshot read the
/// store live, so a `store-state` with `safe_mode: "locked"` came after a
/// snapshot that already showed the recovery.)
#[test]
fn a_store_change_queued_before_the_bus_runs_never_regresses_the_snapshot() {
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let t = LockedBus::start(HeldObserver {
        held: Some((entered_tx, release_rx)),
    });
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("bus thread held before its first take");

    t.retry_succeeds();
    let snapshot = t.snapshot();
    assert_eq!(snapshot.seq, 0);
    assert_eq!(
        (snapshot.revision, snapshot.safe_mode),
        (Config::default().revision, Some(SafeModeReason::Locked))
    );

    release.send(()).unwrap();
    t.marker();
    let sent = store_states(&t.rx);
    let (mut revision, mut safe_mode) = (snapshot.revision, snapshot.safe_mode);
    for event in &sent {
        assert!(event["seq"].as_u64().unwrap() > snapshot.seq);
        let next = event["revision"].as_u64().unwrap();
        assert!(next >= revision, "revision went back: {sent:?}");
        if safe_mode.is_none() {
            assert_eq!(event["safe_mode"], Value::Null, "safe mode came back");
        }
        revision = next;
        safe_mode = (!event["safe_mode"].is_null()).then_some(SafeModeReason::Locked);
    }
    assert_eq!(sent.len(), 1, "only the recovery: {sent:?}");
    assert_eq!((revision, safe_mode), (2, None));
    let after = t.snapshot();
    assert_eq!((after.revision, after.safe_mode), (2, None));
}

/// Review A9: `set_store_sink` itself writes the store's value into the
/// bus's view (replacing whatever the bus started with), on the calling
/// thread, with no `seq`; and with no change after it no `store-state` is
/// sent.
#[test]
fn setting_the_store_sink_makes_the_store_value_the_baseline() {
    let dir = TempDir::new();
    let world = world();
    world.locked.lock().push("active.json");
    write(&dir, "active.json", &bytes(&config_at(2)));
    let mut store = Store::load_with(dir.path(), &env(&world));
    let current = store.store_state();
    assert_eq!(current.safe_mode, Some(SafeModeReason::Locked));
    let (tx, rx) = std::sync::mpsc::channel();
    let stale = crate::events::StoreState {
        revision: current.revision + 40,
        safe_mode: None,
    };
    let bus = crate::events::EventBus::start(
        &store.runtime,
        stale,
        EventChannel(tx),
        Box::new(HeldObserver { held: None }),
    );
    store.set_store_sink(bus.store_sink());
    let view = bus.view();
    assert_eq!((view.seq, view.store), (0, current));
    // An unchanged reload reports the same value: nothing is sent.
    store.apply_retry(load_or_fallback(&dir, &env(&world)));
    (bus.problem_sink())(ProblemKind::AutostartRepaired {}.into());
    assert_eq!(store_states(&rx), Vec::<Value>::new());
}

/// Every startup check passed.
fn passing_self_test() -> SelfTestReport {
    let checks: Vec<_> = CheckName::ALL
        .into_iter()
        .map(|name| SelfTestCheck {
            name,
            passed: true,
            reason: None,
            elapsed_ms: 1,
        })
        .collect();
    SelfTestReport {
        passed: true,
        checks,
    }
}

/// Review A6-1: a store that started in SafeMode(Locked) whose retry
/// succeeds before the startup self-test decides is just a store out of
/// safe mode: the checks pass, auto-listen is on, so it listens.
#[test]
fn a_locked_retry_before_the_self_test_decision_listens_as_usual() {
    let (dir, world, mut store) = store_in(Some(SafeModeReason::Locked));
    store.runtime.publish_health(healthy());
    // Read when startup begins its wait, as `startup_self_test` does.
    let seen = store.manual_changes();
    world.locked.lock().clear();
    store.apply_retry(retry_locked(&dir, &env(&world)).unwrap());
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.runtime.pause_reasons(), PauseReasons::MANUAL);

    let outcome = finish_self_test(&mut store, &passing_self_test(), true, seen, || true);
    assert!(matches!(outcome, StartupListen::Listening), "{outcome:?}");
    assert!(store.runtime.pause_reasons().is_empty());
    assert!(store.runtime.engine_state().listening);
}

/// Review A6-1: the decision came first (safe mode: no auto-listen), the
/// retry succeeds after it. Leaving safe mode does not make up for the
/// missed auto-listen: still paused, MANUAL set, until the user resumes.
#[test]
fn a_locked_retry_after_the_self_test_decision_stays_paused() {
    let (dir, world, mut store) = store_in(Some(SafeModeReason::Locked));
    store.runtime.publish_health(healthy());
    let seen = store.manual_changes();
    let outcome = finish_self_test(&mut store, &passing_self_test(), true, seen, || true);
    assert!(matches!(outcome, StartupListen::SafeMode), "{outcome:?}");

    world.locked.lock().clear();
    store.apply_retry(retry_locked(&dir, &env(&world)).unwrap());
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.runtime.pause_reasons(), PauseReasons::MANUAL);
    assert!(!store.runtime.engine_state().listening);
    assert!(store.engine.paused.load(Ordering::SeqCst));
    // The user's own resume is what starts listening.
    store.set_manual_pause(false, || true).unwrap();
    assert!(store.runtime.engine_state().listening);
}

/// A data directory as 0.3.1 leaves it, in a byte layout 0.4 never writes
/// (compact JSON, CRLF, `[ ]`), so a copy that went through serde would show.
/// Plus a `prefs.json`: 0.3.1 never reads it (the file is new in 0.4), but
/// one that 0.4 wrote before the backup is copied along.
fn legacy_031_dir(dir: &Path) {
    let compact = |config: &Config| {
        let mut out = serde_json::to_vec(config).unwrap();
        out.extend_from_slice(b"\r\n");
        out
    };
    write(dir, "active.json", &compact(&config_at(3)));
    write(dir, "draft.json", &compact(&config_at(3)));
    write(dir, "migration-unresolved.json", b"[]\r\n");
    write(dir, "revisions/1.json", &compact(&config_at(1)));
    write(dir, "revisions/2.json", &compact(&config_at(2)));
    write(dir, "revisions/2.metadata.json", b"[ ]\r\n");
    write(dir, "revisions/3.metadata.json", b"[ ]\r\n");
    write(dir, "revisions/index.json", b"{\"legacy\":true}\r\n");
    write(dir, "prefs.json", b"{}\r\n");
}
/// What a backup of [`legacy_031_dir`] holds: everything except
/// `revisions/1.json`, which is neither the current nor the previous revision.
const LEGACY_BACKED_UP: [&str; 8] = [
    "active.json",
    "draft.json",
    "migration-unresolved.json",
    "prefs.json",
    "revisions/2.json",
    "revisions/2.metadata.json",
    "revisions/3.metadata.json",
    "revisions/index.json",
];
fn rel(name: &str) -> PathBuf {
    name.split('/')
        .fold(PathBuf::new(), |path, part| path.join(part))
}
fn backup_dir(dir: &Path) -> PathBuf {
    dir.join("backup-pre-0.4")
}
/// `names` with their content in `files`.
fn subset(files: &BTreeMap<PathBuf, Vec<u8>>, names: &[&str]) -> BTreeMap<PathBuf, Vec<u8>> {
    names
        .iter()
        .map(|name| (rel(name), files[&rel(name)].clone()))
        .collect()
}
fn backup_temps(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("backup-pre-0.4.tmp-"))
        .collect()
}
fn backup_failed(store: &Store) -> usize {
    let kind = ProblemKind::BackupFailed {
        backup: "backup-pre-0.4".into(),
    };
    store
        .startup_problems()
        .iter()
        .filter(|p| p.kind == kind)
        .count()
}

/// R6 (plan §9, B-43): the first 0.4 write into a 0.3.1 directory (the
/// metadata snapshot at load) is preceded by a byte-for-byte backup of the
/// files 0.3.1 reads; later writes, in this session or the next, make none.
#[test]
fn first_write_backs_up_a_031_directory_byte_for_byte() {
    let dir = TempDir::new();
    let world = world();
    legacy_031_dir(&dir);
    let before = files(&dir);

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.active, config_at(3));
    assert_eq!(
        assert_valid_backup(&backup_dir(&dir)),
        subset(&before, &LEGACY_BACKED_UP)
    );
    assert_eq!(
        meta_on_disk(&dir).backup_pre_0_4_dir.as_deref(),
        Some("backup-pre-0.4")
    );
    // The snapshot rewrote it after the backup was made.
    assert_ne!(
        fs::read(dir.join("revisions/3.metadata.json")).unwrap(),
        before[&rel("revisions/3.metadata.json")]
    );
    assert!(backup_temps(&dir).is_empty());
    assert_eq!(meta_on_disk(&dir).backup_pre_0_4, Some(true));
    assert_eq!(backup_failed(&store), 0);

    // Without the backup, the next writes do not make it again.
    fs::remove_dir_all(backup_dir(&dir)).unwrap();
    store.save(metadata_config(&["b"])).unwrap();
    store.discard().unwrap();
    store.publish(metadata_config(&["b"])).unwrap();
    store.rollback_previous().unwrap();
    assert!(!backup_dir(&dir).exists());
    clean_exit(store, &world);
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active.revision, 5);
    assert!(!backup_dir(&dir).exists());
    assert!(backup_temps(&dir).is_empty());
}

/// R6: a new data directory has nothing from before 0.4. Nothing is backed
/// up, the mark is set with the first write, and the files 0.4 writes are no
/// reason for a backup at the next start either.
#[test]
fn fresh_directory_gets_no_backup() {
    let dir = TempDir::new();
    let world = world();
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert!(!backup_dir(&dir).exists());
    assert_eq!(meta_on_disk(&dir).backup_pre_0_4, Some(false));
    store.publish(metadata_config(&["a"])).unwrap();
    store.publish(metadata_config(&["b"])).unwrap();
    clean_exit(store, &world);
    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active.revision, 2);
    assert!(!backup_dir(&dir).exists());
    assert!(backup_temps(&dir).is_empty());
    assert_eq!(meta_on_disk(&dir).backup_pre_0_4, Some(false));
}

/// R6: while the backup cannot be made, every write is refused with an
/// `io` error at `backup-pre-0.4`, nothing that 0.3.1 reads changes, the
/// engine keeps running on the active configuration and `backup_failed` is
/// listed once. The next write after the cause is gone backs up and goes
/// through.
#[test]
fn failed_backup_refuses_writes_until_it_succeeds() {
    let dir = TempDir::new();
    let world = world();
    legacy_031_dir(&dir);
    // prefs.json cannot be read: the backup cannot be completed.
    fs::remove_file(dir.join("prefs.json")).unwrap();
    fs::create_dir(dir.join("prefs.json")).unwrap();
    let before = files(&dir);
    let data_files = || {
        let mut now = files(&dir);
        now.remove(Path::new(META_FILE));
        now
    };

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.active, config_at(3));
    assert_eq!(backup_failed(&store), 1);
    // Not even the metadata snapshot was written.
    assert_eq!(data_files(), before);
    assert_eq!(meta_on_disk(&dir).backup_pre_0_4, None);
    listening(&store);

    let refused = |result: Result<(), CommandError>| {
        matches!(
            result,
            Err(CommandError::Io { path: Some(ref path), .. }) if path == "backup-pre-0.4"
        )
    };
    assert!(refused(store.save(metadata_config(&["b"]))));
    assert!(refused(store.discard()));
    assert!(refused(store.publish(metadata_config(&["b"]))));
    assert_eq!(data_files(), before);
    assert!(!backup_dir(&dir).exists());
    assert!(backup_temps(&dir).is_empty(), "the partial copy is removed");
    assert_eq!(backup_failed(&store), 1, "listed once");
    assert_eq!(store.active, config_at(3));
    assert!(store.runtime.engine_state().listening);
    assert!(store.runtime.pause_reasons().is_empty());

    fs::remove_dir(dir.join("prefs.json")).unwrap();
    store.publish(metadata_config(&["b"])).unwrap();
    assert_eq!(store.active.revision, 4);
    let mut expected = LEGACY_BACKED_UP.to_vec();
    expected.retain(|name| *name != "prefs.json");
    assert_eq!(
        assert_valid_backup(&backup_dir(&dir)),
        subset(&before, &expected)
    );
    assert_eq!(backup_failed(&store), 0);
    assert_eq!(meta_on_disk(&dir).backup_pre_0_4, Some(true));
    assert!(store.runtime.engine_state().listening);
}

/// R6: a complete `backup-pre-0.4/` found in the directory (one whose
/// manifest checks out, I-B3) is kept as it is and only the mark is set.
#[test]
fn an_existing_backup_is_never_overwritten() {
    let dir = TempDir::new();
    let world = world();
    legacy_031_dir(&dir);
    let earlier_active: &[u8] = b"from an earlier backup";
    write(&dir, "backup-pre-0.4/active.json", earlier_active);
    let manifest = json!({
        "version": 1,
        "files": [{
            "path": "active.json",
            "len": earlier_active.len(),
            "sha256": Fingerprint::of(earlier_active).sha256,
        }],
    });
    write(
        &dir,
        "backup-pre-0.4/manifest.json",
        &value_bytes(&manifest),
    );
    let earlier = files(&backup_dir(&dir));

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(meta_on_disk(&dir).backup_pre_0_4, Some(true));
    assert_eq!(
        meta_on_disk(&dir).backup_pre_0_4_dir.as_deref(),
        Some("backup-pre-0.4")
    );
    store.publish(metadata_config(&["b"])).unwrap();
    assert_eq!(files(&backup_dir(&dir)), earlier);
    assert!(backup_temps(&dir).is_empty());
    assert!(dated_backups(&dir).is_empty());
}

/// R6: the temp directories an interrupted backup left are removed and the
/// backup is made again from the start.
#[test]
fn an_interrupted_backup_is_made_again() {
    let dir = TempDir::new();
    let world = world();
    legacy_031_dir(&dir);
    let before = files(&dir);
    write(
        &dir,
        "backup-pre-0.4.tmp-20260101-000000/active.json",
        b"cut short",
    );
    write(
        &dir,
        "backup-pre-0.4.tmp-20260101-000000-1/revisions/2.json",
        b"",
    );

    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert!(backup_temps(&dir).is_empty());
    assert_eq!(
        assert_valid_backup(&backup_dir(&dir)),
        subset(&before, &LEGACY_BACKED_UP)
    );
}

/// The files of the backup in `backup` after checking them against its
/// manifest (I-B3), independently of `store::backup`: every file is listed
/// with its length and SHA-256, and nothing else is in the directory.
fn assert_valid_backup(backup: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    use sha2::{Digest, Sha256};
    let mut contents = files(backup);
    let manifest = contents
        .remove(Path::new("manifest.json"))
        .unwrap_or_else(|| panic!("{} has no manifest", backup.display()));
    let manifest: Value = serde_json::from_slice(&manifest).unwrap();
    let listed: BTreeMap<PathBuf, (u64, String)> = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                rel(entry["path"].as_str().unwrap()),
                (
                    entry["len"].as_u64().unwrap(),
                    entry["sha256"].as_str().unwrap().to_string(),
                ),
            )
        })
        .collect();
    assert_eq!(
        listed.keys().collect::<Vec<_>>(),
        contents.keys().collect::<Vec<_>>()
    );
    for (path, content) in &contents {
        let sha256: String = Sha256::digest(content)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let expected = (content.len() as u64, sha256);
        assert_eq!(listed[path], expected, "{}", path.display());
    }
    contents
}
/// `store-meta.json` as JSON, fields of any release included.
fn meta_value(dir: &Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join(META_FILE)).unwrap()).unwrap()
}
/// The names of the dated backups (`backup-pre-0.4-<UTC>`) in `dir`.
fn dated_backups(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("backup-pre-0.4-"))
        .collect();
    names.sort();
    names
}
fn problem_kinds(store: &Store) -> Vec<&ProblemKind> {
    store.startup_problems().iter().map(|p| &p.kind).collect()
}
/// `active.json` damaged in the way 0.3.1 could leave it (cut short, CRLF).
const LEGACY_CUT_SHORT: &[u8] = b"{\"version\":1,\"rules\":[\r\n";

/// Review I-R1 (test 1): a publish that fails after store-meta.json was
/// written, at `active.json`, does not change the rollback target of the
/// revision that stays active. Revision 3 was published over revision 1
/// (recovered as last-known-good); publishing 4 fails; after a restart
/// rollback still returns to 1, not to the guessed 2.
#[test]
fn a_failed_publish_keeps_the_rollback_target_of_the_committed_revision() {
    let dir = TempDir::new();
    let world = world();
    let one = write_revision(&dir, 1, "one");
    write_revision(&dir, 2, "two");
    write(&dir, "active.json", TRUNCATED);
    let meta = StoreMeta {
        last_good_revision: Some(1),
        ..StoreMeta::default()
    };
    atomic_json(&dir.join(META_FILE), &meta).unwrap();
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active, one);
    store.publish(metadata_config(&["three"])).unwrap();
    assert_eq!(store.active.revision, 3);
    let target = rollback_target(&store);
    assert_eq!(target.0.as_ref().unwrap().rules, one.rules);
    clean_exit(store, &world);

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(rollback_target(&store), target);
    let committed = fs::read(dir.join("active.json")).unwrap();
    // The temp file of active.json cannot be created: every write before it,
    // store-meta.json included, goes through.
    let blocker = dir.join("active.json.tmp");
    fs::create_dir(&blocker).unwrap();
    assert!(store.publish(metadata_config(&["four"])).is_err());
    assert_eq!(fs::read(dir.join("active.json")).unwrap(), committed);
    clean_exit(store, &world);
    fs::remove_dir(&blocker).unwrap();

    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active.revision, 3);
    assert_eq!(rollback_target(&store), target);
}

/// Review I-R2 (test 2): a record applies only to the `active.json` it was
/// written for. Revision 3 (published over 1, no snapshot of its own yet)
/// is lost with a damaged `active.json` while store-meta.json cannot be
/// read; recovered from 2, the next publish is revision 3 again and its
/// record cannot be written. Once store-meta.json reads again, the old
/// record for "3" does not describe this revision 3: rollback returns to 2.
/// (Since R7-2 the number is reused only when nothing in `revisions/` uses
/// it any more: here `revisions/3.metadata.json` went away as well.)
#[test]
fn a_record_for_a_reused_revision_number_is_ignored() {
    let dir = TempDir::new();
    let world = world();
    write_revision(&dir, 1, "one");
    let two = write_revision(&dir, 2, "two");
    write(&dir, "active.json", TRUNCATED);
    let meta = StoreMeta {
        last_good_revision: Some(1),
        ..StoreMeta::default()
    };
    atomic_json(&dir.join(META_FILE), &meta).unwrap();
    let mut store = Store::load_with(dir.path(), &env(&world));
    store.publish(metadata_config(&["three"])).unwrap();
    assert_eq!(store.active.revision, 3);
    clean_exit(store, &world);

    fs::remove_file(dir.join("revisions/3.metadata.json")).unwrap();
    write(&dir, "active.json", TRUNCATED);
    assert!(!dir.join("revisions/3.json").exists());
    world.locked.lock().push(META_FILE);
    let meta_before = fs::read(dir.join(META_FILE)).unwrap();
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active, two);
    store.publish(metadata_config(&["three again"])).unwrap();
    assert_eq!(store.active.revision, 3);
    assert_eq!(store.previous, Some(two.clone()));
    clean_exit(store, &world);
    assert_eq!(fs::read(dir.join(META_FILE)).unwrap(), meta_before);
    world.locked.lock().clear();

    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active.revision, 3);
    assert_eq!(store.active.rules, metadata_config(&["three again"]).rules);
    assert_eq!(store.previous, Some(two));
}

/// Review R7-2: test 2 with the configuration published again unchanged.
/// Its bytes as revision 3 would be those of the lost revision 3, so the
/// old record for "3" would match them. `revisions/3.metadata.json` still
/// uses the number, so the publish is revision 4 instead: rollback returns
/// to 2 before and after the restart, and that metadata keeps its bytes.
#[test]
fn an_unchanged_republish_does_not_reuse_the_lost_revision_number() {
    let dir = TempDir::new();
    let world = world();
    write_revision(&dir, 1, "one");
    let two = write_revision(&dir, 2, "two");
    write(&dir, "active.json", TRUNCATED);
    let meta = StoreMeta {
        last_good_revision: Some(1),
        ..StoreMeta::default()
    };
    atomic_json(&dir.join(META_FILE), &meta).unwrap();
    let mut store = Store::load_with(dir.path(), &env(&world));
    store.publish(metadata_config(&["three"])).unwrap();
    assert_eq!(store.active.revision, 3);
    clean_exit(store, &world);
    let metadata_of_three = fs::read(dir.join("revisions/3.metadata.json")).unwrap();

    write(&dir, "active.json", TRUNCATED);
    assert!(!dir.join("revisions/3.json").exists());
    world.locked.lock().push(META_FILE);
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.active, two);
    store.publish(metadata_config(&["three"])).unwrap();
    assert_eq!(store.previous, Some(two.clone()), "before the restart");
    let revision = store.active.revision;
    clean_exit(store, &world);
    world.locked.lock().clear();

    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.previous, Some(two), "after the restart");
    assert_eq!((revision, store.active.revision), (4, 4));
    assert_eq!(
        fs::read(dir.join("revisions/3.metadata.json")).unwrap(),
        metadata_of_three
    );
}

/// Review R7-2: a number that only `revisions/<n>.metadata.json` uses is
/// not given out again; that file keeps its bytes.
#[test]
fn a_revision_number_used_only_by_metadata_is_not_given_out() {
    const METADATA: &[u8] = b"[{\"kept\":\"as it was\"}]";
    let dir = TempDir::new();
    let world = world();
    write(&dir, "revisions/5.metadata.json", METADATA);
    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    store.publish(metadata_config(&["one"])).unwrap();
    assert_eq!(store.active.revision, 6);
    assert_eq!(
        fs::read(dir.join("revisions/5.metadata.json")).unwrap(),
        METADATA
    );
}

/// Review R7-3, diagnostic only: when the backup directory store-meta.json
/// names is gone, the load logs `store.backup_missing` with that name and
/// writes go on; with the directory there nothing is logged.
#[test]
fn a_missing_recorded_backup_is_only_logged() {
    use crate::diagnostics::logging::{
        test_support::{read_logs, scoped},
        DEFAULT_LEVEL,
    };
    const RECORDED: &str = "backup-pre-0.4-20260101-000000";
    let dir = TempDir::new();
    let world = world();
    let meta = StoreMeta {
        backup_pre_0_4: Some(true),
        backup_pre_0_4_dir: Some(RECORDED.into()),
        ..StoreMeta::default()
    };
    atomic_json(&dir.join(META_FILE), &meta).unwrap();
    let load_logged = || {
        let logs = TempDir::new();
        let built = scoped(&logs, DEFAULT_LEVEL);
        let store = tracing::subscriber::with_default(built.subscriber, || {
            Store::load_with(dir.path(), &env(&world))
        });
        drop(built.guard);
        let missing: Vec<String> = read_logs(&logs)
            .lines()
            .filter(|line| line.contains("event=\"store.backup_missing\""))
            .map(str::to_owned)
            .collect();
        (store, missing)
    };

    let (mut store, missing) = load_logged();
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(missing[0].contains(RECORDED), "{}", missing[0]);
    store.publish(metadata_config(&["one"])).unwrap();
    assert!(!dir.join(RECORDED).exists());
    clean_exit(store, &world);

    fs::create_dir(dir.join(RECORDED)).unwrap();
    let (_store, missing) = load_logged();
    assert_eq!(missing, Vec::<String>::new());
}

/// Review I-B1 (test 3): at the first 0.4 start a damaged `active.json` is
/// not renamed by the load. The backup holds its original bytes under the
/// name `active.json`; the quarantine copy appears only once that backup
/// is complete.
#[test]
fn a_corrupt_active_is_backed_up_before_it_is_quarantined() {
    let dir = TempDir::new();
    let world = world();
    legacy_031_dir(&dir);
    write(&dir, "active.json", LEGACY_CUT_SHORT);
    let before = files(&dir);
    let data_files = || {
        let mut now = files(&dir);
        now.remove(Path::new(META_FILE));
        now
    };

    // The load alone (no backup yet) leaves every file where 0.3.1 reads it.
    let report = load_or_fallback(&dir, &env(&world));
    assert_eq!(
        report.outcome,
        LoadOutcome::Recovered {
            from: RecoverySource::Revision,
            revision: Some(2)
        }
    );
    assert!(quarantined(&kinds(&report)).is_empty());
    assert_eq!(data_files(), before);
    world.advance(10 * 60 * 1000);

    let store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    let backup = assert_valid_backup(&backup_dir(&dir));
    assert_eq!(backup[&rel("active.json")], LEGACY_CUT_SHORT);
    assert_eq!(quarantined(&problem_kinds(&store)), ["active.json"]);
    assert!(!dir.join("active.json").exists());
    let quarantine: Vec<Vec<u8>> = quarantine_files(&dir).into_values().collect();
    assert_eq!(quarantine, [LEGACY_CUT_SHORT.to_vec()]);
    assert_nothing_lost(&before, &files(&dir));
}

/// Review I-B1 (test 4): while the backup fails, the damaged `active.json`
/// is not renamed either: every file 0.3.1 reads stays byte for byte, the
/// write is refused, and the next write tries both again.
#[test]
fn a_failed_backup_leaves_a_corrupt_active_in_place() {
    let dir = TempDir::new();
    let world = world();
    legacy_031_dir(&dir);
    write(&dir, "active.json", LEGACY_CUT_SHORT);
    // prefs.json cannot be read: the backup cannot be completed.
    fs::remove_file(dir.join("prefs.json")).unwrap();
    fs::create_dir(dir.join("prefs.json")).unwrap();
    let before = files(&dir);
    let data_files = || {
        let mut now = files(&dir);
        now.remove(Path::new(META_FILE));
        now
    };

    let mut store = Store::load_with(dir.path(), &env(&world));
    assert_eq!(store.safe_mode(), None);
    assert_eq!(store.active.revision, 2);
    assert_eq!(backup_failed(&store), 1);
    assert!(quarantined(&problem_kinds(&store)).is_empty());
    assert_eq!(data_files(), before);
    let refused = |result: Result<(), CommandError>| {
        matches!(
            result,
            Err(CommandError::Io { path: Some(ref path), .. }) if path == "backup-pre-0.4"
        )
    };
    assert!(refused(store.save(metadata_config(&["b"]))));
    assert!(refused(store.publish(metadata_config(&["b"]))));
    assert_eq!(data_files(), before);

    fs::remove_dir(dir.join("prefs.json")).unwrap();
    store.save(metadata_config(&["b"])).unwrap();
    assert_eq!(backup_failed(&store), 0);
    let backup = assert_valid_backup(&backup_dir(&dir));
    assert_eq!(backup[&rel("active.json")], LEGACY_CUT_SHORT);
    assert!(!dir.join("active.json").exists());
    assert_eq!(quarantined(&problem_kinds(&store)), ["active.json"]);
    store.publish(store.draft.clone()).unwrap();
    // Review R7-2: 0.3.1's `revisions/3.metadata.json` keeps the number
    // taken (the lost revision 3), and keeps its bytes.
    assert_eq!(store.active.revision, 4);
    assert_eq!(
        fs::read(dir.join("revisions/3.metadata.json")).unwrap(),
        b"[ ]\r\n"
    );
}

/// Review I-B3 (test 5): a `backup-pre-0.4` without a valid manifest (empty,
/// cut short, or a file) is never taken as the backup and never changed or
/// removed: a dated `backup-pre-0.4-<UTC>/` with a manifest is made beside
/// it, and store-meta.json names it. Later writes and launches keep both.
#[test]
fn an_incomplete_existing_backup_is_kept_and_a_dated_one_made() {
    for existing in ["empty", "partial", "file"] {
        let dir = TempDir::new();
        let world = world();
        legacy_031_dir(&dir);
        let before = files(&dir);
        match existing {
            "empty" => fs::create_dir(backup_dir(&dir)).unwrap(),
            "partial" => write(&dir, "backup-pre-0.4/active.json", b"cut short"),
            _ => write(&dir, "backup-pre-0.4", b"not a directory"),
        }
        let original = || {
            let mut entries: BTreeMap<PathBuf, Vec<u8>> = files(&dir)
                .into_iter()
                .filter(|(path, _)| path.starts_with("backup-pre-0.4"))
                .collect();
            if backup_dir(&dir).is_dir() {
                let count = fs::read_dir(backup_dir(&dir)).unwrap().count();
                entries.insert(PathBuf::from("<entries>"), vec![count as u8]);
            }
            entries
        };
        let earlier = original();

        let mut store = Store::load_with(dir.path(), &env(&world));
        assert_eq!(store.safe_mode(), None, "{existing}");
        assert_eq!(backup_failed(&store), 0, "{existing}");
        assert_eq!(original(), earlier, "{existing}");
        let dated = dated_backups(&dir);
        assert_eq!(dated.len(), 1, "{existing}");
        let backup = assert_valid_backup(&dir.join(&dated[0]));
        assert_eq!(backup, subset(&before, &LEGACY_BACKED_UP), "{existing}");
        let meta = meta_value(&dir);
        assert_eq!(meta["backup_pre_0_4"], true, "{existing}");
        assert_eq!(meta["backup_pre_0_4_dir"], dated[0].as_str(), "{existing}");

        store.publish(metadata_config(&["b"])).unwrap();
        clean_exit(store, &world);
        let store = Store::load_with(dir.path(), &env(&world));
        assert_eq!(store.active.revision, 4);
        assert_eq!(original(), earlier, "{existing}");
        assert_eq!(dated_backups(&dir), dated, "{existing}");
        assert!(backup_temps(&dir).is_empty());
    }
}

/// Review I-B2 (test 6): a recovery import after every candidate turned out
/// corrupt is no new data directory. The mark is never `false`, and the
/// backup holds the originals: the damaged candidates this launch found
/// (still unrenamed), and the quarantine copies an earlier launch left.
/// Review R7-1: with them the metadata of the damaged snapshot, which
/// 0.3.1 reads beside it; the publish (revision 2, R7-2) leaves it as it is.
#[test]
fn a_recovery_import_backs_up_the_corrupt_candidates() {
    const METADATA_OF_ONE: &[u8] = b"[{\"kept\":\"as 0.3.1 left it\"}]";
    let import = |dir: &Path, world: &Arc<World>| {
        let mut store = Store::load_with(dir.to_path_buf(), &env(world));
        assert_no_valid_config(&store);
        let imported = metadata_config(&["imported"]);
        store.pending_import = Some((imported.clone(), vec![]));
        store.save(imported).unwrap();
        store.publish(store.draft.clone()).unwrap();
        assert_eq!(store.safe_mode(), None);
        assert_eq!(meta_value(dir)["backup_pre_0_4"], true);
        let backup = assert_valid_backup(&backup_dir(dir));
        assert_eq!(
            backup
                .get(&rel("revisions/1.metadata.json"))
                .map(Vec::as_slice),
            Some(METADATA_OF_ONE),
            "metadata of 1 in the backup"
        );
        assert_eq!(
            fs::read(dir.join("revisions/1.metadata.json")).unwrap(),
            METADATA_OF_ONE,
            "metadata of 1 after the save and publish"
        );
        assert_eq!(store.active.revision, 2);
        backup
    };

    let dir = TempDir::new();
    let world = world();
    write(&dir, "active.json", TRUNCATED);
    write(&dir, "revisions/1.json", b"\xFF");
    write(&dir, "revisions/1.metadata.json", METADATA_OF_ONE);
    write(&dir, "previous.json", b"[]");
    let before = files(&dir);
    let backup = import(&dir, &world);
    let names = [
        "active.json",
        "revisions/1.json",
        "revisions/1.metadata.json",
        "previous.json",
    ];
    assert_eq!(subset(&backup, &names), before);
    assert_eq!(quarantine_files(&dir).len(), 3);
    assert_nothing_lost(&before, &files(&dir));

    let dir = TempDir::new();
    let world = self::world();
    write(&dir, "active.corrupt-20260101-000000.json", TRUNCATED);
    write(&dir, "revisions/1.corrupt-20260101-000000.json", b"\xFF");
    write(&dir, "revisions/1.metadata.json", METADATA_OF_ONE);
    let before = files(&dir);
    let backup = import(&dir, &world);
    let names = [
        "active.corrupt-20260101-000000.json",
        "revisions/1.corrupt-20260101-000000.json",
        "revisions/1.metadata.json",
    ];
    assert_eq!(subset(&backup, &names), before);
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

//! Team phrase sync end to end against a local HTTP server (`std::net::TcpListener`),
//! and the fake server the download tests share.
use super::{
    cache,
    fetch::{endpoint, FetchLimits},
    *,
};
use crate::{
    app_state::Managed,
    error::{CommandError, SafeModeReason},
    events::CloudSink,
    store::prefs,
};
use quicker_domain::*;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

/// One canned answer: written `delay` after the request was read, then the connection
/// is closed.
pub struct Reply {
    delay: Duration,
    bytes: Vec<u8>,
}

impl Reply {
    pub fn now(bytes: Vec<u8>) -> Self {
        Self::after(Duration::ZERO, bytes)
    }
    pub fn after(delay: Duration, bytes: Vec<u8>) -> Self {
        Self { delay, bytes }
    }
}

/// A complete HTTP/1.1 response with `Content-Length` and `Connection: close`.
pub fn http(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Serves one connection per reply, in order, then stops. Records each request head.
pub struct FakeServer {
    addr: SocketAddr,
    requests: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl FakeServer {
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(parking_lot::Mutex::new(vec![]));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                seen.lock()
                    .push(String::from_utf8_lossy(&head).into_owned());
                std::thread::sleep(reply.delay);
                let _ = stream.write_all(&reply.bytes);
                let _ = stream.flush();
            }
        });
        Self { addr, requests }
    }
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().clone()
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("quicker-cloud-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn phrase(id: &str, trigger: &str, body: &str) -> Value {
    json!({
        "id": id,
        "enabled": true,
        "group": "退款",
        "description": "说明",
        "ignore_case": false,
        "match": { "kind": "literal", "value": trigger },
        "trigger_mode": "terminator",
        "terminator": null,
        "ime_erasure": null,
        "action": { "type": "text", "method": "paste", "source": { "type": "inline", "value": body } },
        "parameter": { "type": "default_match" },
        "notification": { "enabled": false, "message": "" }
    })
}

fn table(revision: u64, rules: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "format": "quicktext-cloud-phrases",
        "schema_version": 1,
        "revision": revision,
        "updated_at": "2026-10-28T08:30:00+00:00",
        "rules": rules
    }))
    .unwrap()
}

fn ok(etag: &str, body: &[u8]) -> Reply {
    Reply::now(http(
        "200 OK",
        &[("ETag", etag), ("Content-Type", "application/json")],
        body,
    ))
}

fn personal_rule(id: &str, trigger: &str) -> Rule {
    Rule {
        id: id.into(),
        match_spec: MatchSpec {
            kind: MatchKind::Literal,
            value: trigger.into(),
        },
        action: Action::Text {
            method: OutputMethod::Paste,
            source: ContentSource::inline("personal body"),
        },
        ..Rule::default()
    }
}

/// A store as setup builds it: loaded, then the team phrases from prefs and cache.
fn load(dir: &std::path::Path) -> Managed {
    let mut store = Store::load(dir.to_path_buf());
    init(&mut store);
    Managed::new(store)
}

fn hub() -> CloudSync {
    CloudSync::with_limits(FetchLimits {
        timeout: Duration::from_millis(1500),
        max_bytes: 64 * 1024,
    })
}

fn settings(base_url: &str, key: Option<&str>) -> CloudSettingsInput {
    CloudSettingsInput {
        enabled: true,
        base_url: base_url.into(),
        key: key.map(str::to_string),
        auto_sync_minutes: 30,
    }
}

fn run(store: &Managed, hub: &CloudSync, trigger: Trigger) -> CloudState {
    tauri::async_runtime::block_on(sync(store, hub, trigger))
}

/// A cache file as a sync from `base_url` writes it.
fn write_cache(dir: &std::path::Path, base_url: &str, revision: u64, rules: Vec<Value>) {
    cache::write(
        dir,
        &cache_source(base_url).unwrap(),
        Some("\"e\""),
        "2026-10-28T00:00:00Z",
        &serde_json::from_slice(&table(revision, rules)).unwrap(),
    )
    .unwrap();
}

/// A cache file with any table bytes, as an older release might have written it.
fn write_cache_unchecked(dir: &std::path::Path, base_url: &str, table: Vec<u8>) {
    cache::write(
        dir,
        &cache_source(base_url).unwrap(),
        Some("\"e\""),
        "2026-10-28T00:00:00Z",
        &serde_json::from_slice(&table).unwrap(),
    )
    .unwrap();
}

fn publish(store: &Managed, rules: Vec<Rule>) {
    let mut s = store.lock();
    let mut config = s.active.clone();
    config.rules = rules;
    s.save_config(config, None).unwrap();
}

fn engine_ids(store: &Managed) -> Vec<String> {
    let s = store.lock();
    let config = s.engine.config.lock().unwrap();
    config.rules.iter().map(|rule| rule.id.clone()).collect()
}

fn compiled_ids(store: &Managed) -> Vec<String> {
    let s = store.lock();
    let compiled = s.engine.compiled.lock().unwrap();
    compiled
        .rules
        .iter()
        .map(|rule| rule.rule.id.clone())
        .collect()
}

fn engine_is_personal(store: &Managed) -> bool {
    let s = store.lock();
    let config = s.engine.config.lock().unwrap();
    let compiled = s.engine.compiled.lock().unwrap();
    *config == s.active && compiled.config == s.active
}

fn statuses(state: &CloudState) -> Vec<(String, CloudRuleStatus)> {
    state
        .rules
        .iter()
        .map(|rule| (rule.id.clone(), rule.status))
        .collect()
}

fn files_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// §4: no subscription, no team phrases: the engine runs exactly the personal
/// configuration, also with a cache file left on disk.
#[test]
fn without_a_subscription_the_engine_runs_the_personal_configuration() {
    let dir = temp_dir();
    let store = load(&dir);
    publish(&store, vec![personal_rule("mine", "pp")]);
    assert!(engine_is_personal(&store));
    let mut off = settings("https://csapi.tsim.tech", None);
    off.enabled = false;
    apply_settings(&store, &hub(), off).unwrap();
    write_cache(
        &dir,
        "https://csapi.tsim.tech",
        3,
        vec![phrase("a", "aa", "x")],
    );
    let store = load(&dir);
    assert!(
        store.lock().cloud.cache.is_some(),
        "the cache of this address is loaded"
    );
    assert!(engine_is_personal(&store));
    let state = store.lock().cloud_state();
    assert!(!state.settings.enabled && !state.settings.has_key);
    assert_eq!(state.settings.auto_sync_minutes, 30);
    assert!(state.rules.is_empty());
    assert_eq!(state.status.revision, None);
    // An automatic sync while off does nothing and records nothing.
    let state = run(&store, &hub(), Trigger::Timer);
    assert_eq!(state.status.last_attempt_at, None);
    assert_eq!(state.status.last_error, None);
    std::fs::remove_dir_all(dir).unwrap();
}

/// §3, §4: a 200 is cached and merged; the personal files and `load_state.active`
/// never see a team phrase; the next sync is conditional (304), a 401 keeps the cache
/// and the engine; a restart without the network runs the cached table.
#[test]
fn a_sync_caches_and_merges_and_the_cache_survives_failures_and_restarts() {
    let dir = temp_dir();
    let store = load(&dir);
    publish(&store, vec![personal_rule("mine", "same")]);
    let active_before = std::fs::read(dir.join("active.json")).unwrap();
    let revisions_before = files_in(&dir.join("revisions"));
    let server = FakeServer::start(vec![
        ok(
            "\"e1\"",
            &table(
                12,
                vec![phrase("a", "aa", "团队正文"), phrase("b", "same", "x")],
            ),
        ),
        Reply::now(http("304 Not Modified", &[("ETag", "\"e1\"")], b"")),
        Reply::now(http("401 Unauthorized", &[], b"{}")),
    ]);
    let hub = hub();
    apply_settings(
        &store,
        &hub,
        settings(&server.base_url(), Some("qtk_1.secret")),
    )
    .unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(state.status.last_error, None, "{:?}", state.status);
    assert!(!state.status.syncing);
    assert_eq!(state.status.revision, Some(12));
    assert_eq!(
        state.status.updated_at.as_deref(),
        Some("2026-10-28T08:30:00+00:00")
    );
    assert!(state.status.last_success_at.is_some());
    assert_eq!(
        statuses(&state),
        [
            ("a".to_string(), CloudRuleStatus::Active),
            ("b".to_string(), CloudRuleStatus::Active)
        ]
    );
    // The phrase with the personal rule's trigger wins: the personal rule is not in the
    // engine (it is still in `active`, below).
    assert!(state.rules[0].overrides.is_empty());
    assert_eq!(state.rules[1].overrides, ["mine"]);
    assert_eq!(engine_ids(&store), ["cloud:a", "cloud:b"]);
    assert_eq!(compiled_ids(&store), ["cloud:a", "cloud:b"]);
    {
        let s = store.lock();
        // The revision stays the personal one; `active` and `load_state` are personal.
        assert_eq!(s.engine.config.lock().unwrap().revision, s.active.revision);
        assert_eq!(s.active.rules.len(), 1);
        let shown = serde_json::to_string(&s.state()).unwrap();
        assert!(!shown.contains("cloud:") && !shown.contains("团队正文"));
        assert!(!s.export_snapshot().unwrap().contains("团队正文"));
    }
    assert_eq!(
        std::fs::read(dir.join("active.json")).unwrap(),
        active_before
    );
    assert_eq!(files_in(&dir.join("revisions")), revisions_before);
    let cached: Value =
        serde_json::from_slice(&std::fs::read(dir.join("cloud-phrases.json")).unwrap()).unwrap();
    assert_eq!(cached["etag"], "\"e1\"");
    assert_eq!(
        cached["source"].as_str(),
        cache_source(&server.base_url()).as_deref()
    );
    assert_eq!(cached["table"]["revision"], 12);
    assert_eq!(
        Some(cached["synced_at"].as_str().unwrap()),
        state.status.last_success_at.as_deref()
    );

    // 304: conditional on the cached ETag; nothing else changes.
    let state = run(&store, &hub, Trigger::Timer);
    assert_eq!(state.status.last_error, None);
    assert_eq!(state.status.revision, Some(12));
    assert_eq!(engine_ids(&store), ["cloud:a", "cloud:b"]);
    let requests = server.requests();
    assert!(
        requests[0].contains("Bearer qtk_1.secret") || requests[0].contains("bearer qtk_1.secret")
    );
    assert!(!requests[0].to_ascii_lowercase().contains("if-none-match"));
    assert!(requests[1]
        .to_ascii_lowercase()
        .contains("if-none-match: \"e1\""));

    // 401: an error, the cache and the engine stay.
    let state = run(&store, &hub, Trigger::Manual);
    let error = state.status.last_error.clone().unwrap();
    assert_eq!(error.code, code::UNAUTHORIZED);
    assert_eq!(
        error.message,
        "同步密钥无效或已作废，请向管理员索取新的同步密钥"
    );
    assert!(!error.message.contains("secret") && !error.detail.contains("secret"));
    // Only code and message reach the frontend; the detail is for the log.
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({ "code": "unauthorized", "message": error.message })
    );
    assert_eq!(state.status.revision, Some(12));
    assert_eq!(engine_ids(&store), ["cloud:a", "cloud:b"]);
    assert!(dir.join("cloud-phrases.json").exists());
    // The key never reaches the state.
    assert!(!serde_json::to_string(&state).unwrap().contains("qtk_1"));
    assert!(state.settings.has_key);

    // Restart without the network: the cache is loaded before anything runs.
    drop(store);
    let store = load(&dir);
    assert_eq!(engine_ids(&store), ["cloud:a", "cloud:b"]);
    let state = store.lock().cloud_state();
    assert_eq!(state.status.revision, Some(12));
    assert_eq!(
        state.status.last_success_at.as_deref(),
        cached["synced_at"].as_str()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// §3: every failure keeps the cache and the engine, and says why.
#[test]
fn failures_keep_the_cache_and_the_engine() {
    let dir = temp_dir();
    let store = load(&dir);
    let mut newer: Value = serde_json::from_slice(&table(13, vec![])).unwrap();
    newer["schema_version"] = json!(2);
    let mut not_ours: Value = serde_json::from_slice(&table(13, vec![])).unwrap();
    not_ours["format"] = json!("something-else");
    let server = FakeServer::start(vec![
        ok("\"e1\"", &table(12, vec![phrase("a", "aa", "x")])),
        ok("\"e2\"", b"<html>maintenance</html>"),
        ok("\"e3\"", &serde_json::to_vec(&newer).unwrap()),
        ok("\"e4\"", &serde_json::to_vec(&not_ours).unwrap()),
        Reply::now(http("500 Internal Server Error", &[], b"")),
        ok("\"e5\"", &vec![b' '; 70 * 1024]),
        Reply::after(Duration::from_secs(4), http("200 OK", &[], b"{}")),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    assert_eq!(run(&store, &hub, Trigger::Manual).status.last_error, None);
    for expected in [
        code::INVALID_FORMAT,
        code::SCHEMA_NEWER,
        code::INVALID_FORMAT,
        code::HTTP_STATUS,
        code::TOO_LARGE,
        code::TIMEOUT,
    ] {
        let state = run(&store, &hub, Trigger::Manual);
        assert_eq!(
            state.status.last_error.as_ref().map(|e| e.code),
            Some(expected)
        );
        assert!(!state.status.syncing);
        assert_eq!(state.status.revision, Some(12), "{expected}");
        assert_eq!(engine_ids(&store), ["cloud:a"], "{expected}");
    }
    let cached: Value =
        serde_json::from_slice(&std::fs::read(dir.join("cloud-phrases.json")).unwrap()).unwrap();
    assert_eq!(cached["etag"], "\"e1\"");
    std::fs::remove_dir_all(dir).unwrap();
}

/// §3: `not_configured` and `invalid_url` before anything is sent.
#[test]
fn a_sync_that_cannot_run_says_why() {
    let dir = temp_dir();
    let store = load(&dir);
    let hub = hub();
    // Subscribed by default; a manual sync without a key says so, a timer one stays silent.
    let state = run(&store, &hub, Trigger::Startup);
    assert!(state.status.last_error.is_none());
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(
        state.status.last_error.as_ref().unwrap().message,
        "未设置同步密钥"
    );
    assert!(state.status.last_attempt_at.is_some());
    apply_settings(&store, &hub, settings("https://example.invalid", None)).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(
        state.status.last_error.as_ref().map(|e| e.code),
        Some(code::NOT_CONFIGURED)
    );
    assert_eq!(
        state.status.last_error.as_ref().unwrap().message,
        "未设置同步密钥"
    );
    // A stored address that is no longer acceptable (edited by hand).
    store.lock().cloud.prefs.base_url = "http://example.com".into();
    store.lock().cloud.prefs.key = "k".into();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(
        state.status.last_error.as_ref().map(|e| e.code),
        Some(code::INVALID_URL)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// §4: saving the personal configuration re-merges (a new personal rule with a team
/// phrase's trigger is replaced by the phrase) and never writes a team phrase.
#[test]
fn a_personal_save_remerges_and_stays_personal() {
    let dir = temp_dir();
    let store = load(&dir);
    let server = FakeServer::start(vec![ok(
        "\"e1\"",
        &table(1, vec![phrase("a", "aa", "x"), phrase("b", "bb", "y")]),
    )]);
    let hub = hub();
    let (sink, events) = CloudSink::detached();
    store.lock().set_cloud_sink(sink);
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    publish(&store, vec![personal_rule("first", "ff")]);
    run(&store, &hub, Trigger::Manual);
    assert_eq!(engine_ids(&store), ["first", "cloud:a", "cloud:b"]);
    publish(&store, vec![personal_rule("mine", "bb")]);
    assert_eq!(engine_ids(&store), ["cloud:a", "cloud:b"]);
    // The save is reported as `cloud-state` (after the earlier events, in order): the
    // phrase `b` now lists the personal rule it replaces.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the save sends cloud-state");
        // Earlier events may come before the table is loaded.
        if event
            .rules
            .get(1)
            .is_some_and(|rule| rule.overrides == ["mine"])
        {
            assert_eq!(
                statuses(&event),
                [
                    ("a".to_string(), CloudRuleStatus::Active),
                    ("b".to_string(), CloudRuleStatus::Active),
                ]
            );
            break;
        }
    }
    assert!(events.try_recv().is_err(), "nothing after the save's event");
    let active = std::fs::read_to_string(dir.join("active.json")).unwrap();
    assert!(!active.contains("cloud:"));
    // Rollback re-merges as well.
    store.lock().rollback_previous().unwrap();
    assert_eq!(engine_ids(&store), ["first", "cloud:a", "cloud:b"]);
    std::fs::remove_dir_all(dir).unwrap();
}

/// §2, §4: local disable and enable; ids that match nothing are dropped after the
/// next successful sync.
#[test]
fn local_disable_switches_phrases_and_stale_ids_are_dropped() {
    let dir = temp_dir();
    let store = load(&dir);
    let server = FakeServer::start(vec![
        ok(
            "\"e1\"",
            &table(1, vec![phrase("a", "aa", "x"), phrase("b", "bb", "y")]),
        ),
        Reply::now(http("304 Not Modified", &[], b"")),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    run(&store, &hub, Trigger::Manual);
    let state = set_rules_disabled(&store, &hub, vec!["a".into(), "ghost".into()], true).unwrap();
    assert_eq!(
        statuses(&state),
        [
            ("a".to_string(), CloudRuleStatus::DisabledLocal),
            ("b".to_string(), CloudRuleStatus::Active)
        ]
    );
    assert_eq!(engine_ids(&store), ["cloud:b"]);
    let stored = prefs::load_prefs(&dir).unwrap().app.cloud.unwrap();
    assert_eq!(stored.disabled_ids, ["a", "ghost"]);
    run(&store, &hub, Trigger::Manual);
    let stored = prefs::load_prefs(&dir).unwrap().app.cloud.unwrap();
    assert_eq!(stored.disabled_ids, ["a"]);
    assert_eq!(store.lock().cloud.prefs.disabled_ids, ["a"]);
    let state = set_rules_disabled(&store, &hub, vec!["a".into()], false).unwrap();
    assert!(state
        .rules
        .iter()
        .all(|rule| rule.status == CloudRuleStatus::Active));
    assert_eq!(engine_ids(&store), ["cloud:a", "cloud:b"]);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Safe mode: a sync updates the cache and the statuses; the engine keeps the
/// personal configuration (here the empty stand-in of SafeMode(NoValidConfig)).
#[test]
fn in_safe_mode_a_sync_only_updates_the_cache_and_the_statuses() {
    let dir = temp_dir();
    std::fs::write(dir.join("active.json"), b"{").unwrap();
    let store = load(&dir);
    assert_eq!(
        store.lock().safe_mode(),
        Some(SafeModeReason::NoValidConfig)
    );
    // `broken` passes every check before compiling; only compiling refuses it (an empty
    // terminator override). In safe mode it stays `active`: nothing is compiled.
    let mut broken = phrase("broken", "bb", "x");
    broken["terminator"] = json!({ "keys": [], "consume": true });
    let server = FakeServer::start(vec![ok(
        "\"e1\"",
        &table(1, vec![phrase("a", "aa", "x"), broken]),
    )]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(state.status.last_error, None);
    assert_eq!(
        statuses(&state),
        [
            ("a".to_string(), CloudRuleStatus::Active),
            ("broken".to_string(), CloudRuleStatus::Active)
        ]
    );
    assert!(dir.join("cloud-phrases.json").exists());
    assert!(engine_is_personal(&store));
    assert!(engine_ids(&store).is_empty());
    // Local disable is allowed too; the engine stays personal.
    let state = set_rules_disabled(&store, &hub, vec!["a".into()], true).unwrap();
    assert_eq!(
        statuses(&state),
        [
            ("a".to_string(), CloudRuleStatus::DisabledLocal),
            ("broken".to_string(), CloudRuleStatus::Active)
        ]
    );
    assert!(engine_is_personal(&store));
    // Leaving safe mode (an import published) merges the team phrases.
    set_rules_disabled(&store, &hub, vec!["a".into()], false).unwrap();
    {
        let mut s = store.lock();
        let mut config = s.active.clone();
        config.rules = vec![personal_rule("mine", "pp")];
        s.pending_import = Some((config.clone(), vec![]));
        s.publish(config).unwrap();
        assert_eq!(s.safe_mode(), None);
    }
    assert_eq!(engine_ids(&store), ["mine", "cloud:a"]);
    // Now compiled: `broken` is refused.
    let state = store.lock().cloud_state();
    assert_eq!(state.rules[1].status, CloudRuleStatus::Invalid);
    assert_eq!(state.rules[1].problem.as_deref(), Some("结束键设置无效"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// §5: settings are checked before anything is written; the key is write-only.
#[test]
fn settings_are_validated_and_the_key_is_write_only() {
    let dir = temp_dir();
    let store = load(&dir);
    let hub = hub();
    let mut bad = settings("http://csapi.tsim.tech", Some("has space"));
    bad.auto_sync_minutes = 7;
    let Err(CommandError::Validation { diagnostics }) = apply_settings(&store, &hub, bad) else {
        panic!("expected a validation error");
    };
    let paths: Vec<_> = diagnostics.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "settings.base_url",
            "settings.key",
            "settings.auto_sync_minutes"
        ]
    );
    assert!(!format!("{diagnostics:?}").contains("has space"));
    assert!(!dir.join("prefs.json").exists());

    let state = apply_settings(
        &store,
        &hub,
        settings(" https://csapi.tsim.tech/ ", Some(" qtk_a.b ")),
    )
    .unwrap();
    assert_eq!(state.settings.base_url, "https://csapi.tsim.tech/");
    assert!(state.settings.has_key);
    assert_eq!(store.lock().cloud.prefs.key, "qtk_a.b");
    let written = std::fs::read_to_string(dir.join("prefs.json")).unwrap();
    assert!(written.contains("qtk_a.b"));
    assert!(!serde_json::to_string(&state).unwrap().contains("qtk_a.b"));
    // Absent keeps the key, "" clears it.
    let state = apply_settings(&store, &hub, settings("https://csapi.tsim.tech", None)).unwrap();
    assert!(state.settings.has_key);
    let state =
        apply_settings(&store, &hub, settings("https://csapi.tsim.tech", Some(""))).unwrap();
    assert!(!state.settings.has_key);
    // Debug output never shows a key.
    let shown = format!(
        "{:?} {:?}",
        settings("https://x", Some("qtk_secret")),
        store.lock().cloud.prefs
    );
    assert!(
        !shown.contains("qtk_secret") && !shown.contains("qtk_a.b"),
        "{shown}"
    );
    // The timer follows the interval: 30 minutes, none for 0, none when off.
    let due = hub.next_due().unwrap();
    assert!(due > Instant::now() + Duration::from_secs(29 * 60));
    let mut manual = settings("https://csapi.tsim.tech", None);
    manual.auto_sync_minutes = 0;
    apply_settings(&store, &hub, manual).unwrap();
    assert_eq!(hub.next_due(), None);
    let mut off = settings("https://csapi.tsim.tech", None);
    off.enabled = false;
    apply_settings(&store, &hub, off).unwrap();
    assert_eq!(hub.next_due(), None);
    std::fs::remove_dir_all(dir).unwrap();
}

/// §2: a new server address drops the old cache; a result that arrives after the
/// settings changed is dropped too.
#[test]
fn a_new_address_drops_the_old_table() {
    let dir = temp_dir();
    let store = load(&dir);
    let server = FakeServer::start(vec![
        ok("\"e1\"", &table(1, vec![phrase("a", "aa", "x")])),
        Reply::after(
            Duration::from_millis(1000),
            http(
                "200 OK",
                &[("ETag", "\"e2\"")],
                &table(2, vec![phrase("z", "zz", "x")]),
            ),
        ),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    run(&store, &hub, Trigger::Manual);
    assert_eq!(engine_ids(&store), ["cloud:a"]);
    // A slow download, and the key changes meanwhile: its result is not installed.
    std::thread::scope(|scope| {
        let slow = scope.spawn(|| run(&store, &hub, Trigger::Timer));
        std::thread::sleep(Duration::from_millis(300));
        apply_settings(&store, &hub, settings(&server.base_url(), Some("k2"))).unwrap();
        let state = slow.join().unwrap();
        assert_eq!(state.status.revision, Some(1));
        assert!(!state.status.syncing);
    });
    assert_eq!(engine_ids(&store), ["cloud:a"]);
    let cached: Value =
        serde_json::from_slice(&std::fs::read(dir.join("cloud-phrases.json")).unwrap()).unwrap();
    assert_eq!(cached["etag"], "\"e1\"");
    // Another server: the old table goes, from memory and disk.
    let state = apply_settings(&store, &hub, settings("https://csapi.tsim.tech", None)).unwrap();
    assert!(state.rules.is_empty());
    assert_eq!(state.status.revision, None);
    assert!(engine_is_personal(&store));
    assert!(!dir.join("cloud-phrases.json").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

/// §3: a request made while a sync runs joins it (single flight): one download, the
/// same result.
#[test]
fn a_request_during_a_sync_joins_it() {
    let dir = temp_dir();
    let store = load(&dir);
    let server = FakeServer::start(vec![
        Reply::after(
            Duration::from_millis(1000),
            http(
                "200 OK",
                &[("ETag", "\"e1\"")],
                &table(1, vec![phrase("a", "aa", "x")]),
            ),
        ),
        ok("\"e2\"", &table(2, vec![])),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let (running, joined) = std::thread::scope(|scope| {
        let running = scope.spawn(|| run(&store, &hub, Trigger::Settings));
        std::thread::sleep(Duration::from_millis(300));
        let joined = run(&store, &hub, Trigger::Manual);
        (running.join().unwrap(), joined)
    });
    assert_eq!(server.requests().len(), 1);
    assert_eq!(running, joined);
    assert_eq!(joined.status.revision, Some(1));
    // Not running any more: the next request downloads again.
    assert_eq!(run(&store, &hub, Trigger::Manual).status.revision, Some(2));
    std::fs::remove_dir_all(dir).unwrap();
}

/// §3: a sync for new settings never joins one that started with the old ones.
#[test]
fn a_sync_for_new_settings_does_not_join_the_old_one() {
    let dir = temp_dir();
    let store = load(&dir);
    let server = FakeServer::start(vec![
        Reply::after(
            Duration::from_millis(1000),
            http(
                "200 OK",
                &[("ETag", "\"old\"")],
                &table(1, vec![phrase("a", "aa", "x")]),
            ),
        ),
        ok("\"new\"", &table(2, vec![phrase("b", "bb", "x")])),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k1"))).unwrap();
    let state = std::thread::scope(|scope| {
        let old = scope.spawn(|| run(&store, &hub, Trigger::Timer));
        std::thread::sleep(Duration::from_millis(300));
        apply_settings(&store, &hub, settings(&server.base_url(), Some("k2"))).unwrap();
        // "Sync now" right after saving joins nothing stale either.
        let manual = scope.spawn(|| run(&store, &hub, Trigger::Manual));
        let state = run(&store, &hub, Trigger::Settings);
        old.join().unwrap();
        assert_eq!(manual.join().unwrap(), state);
        state
    });
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("k2"));
    assert_eq!(state.status.revision, Some(2));
    assert_eq!(engine_ids(&store), ["cloud:b"]);
    std::fs::remove_dir_all(dir).unwrap();
}

/// §3: one sync at a time; requests that waited while another sync started after
/// them take its result.
#[test]
fn concurrent_requests_share_one_sync() {
    let dir = temp_dir();
    let store = load(&dir);
    let server = FakeServer::start(vec![
        ok("\"e1\"", &table(1, vec![phrase("a", "aa", "x")])),
        ok("\"e1\"", &table(1, vec![phrase("a", "aa", "x")])),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let gate = hub.gate.blocking_lock();
    let states = std::thread::scope(|scope| {
        let first = scope.spawn(|| run(&store, &hub, Trigger::Manual));
        let second = scope.spawn(|| run(&store, &hub, Trigger::Manual));
        std::thread::sleep(Duration::from_millis(300));
        drop(gate);
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(server.requests().len(), 1);
    assert_eq!(states[0], states[1]);
    assert_eq!(states[0].status.revision, Some(1));
    std::fs::remove_dir_all(dir).unwrap();
}

/// §5: a sync sends `cloud-state` when it starts and when it ends.
#[test]
fn a_sync_reports_its_start_and_its_end() {
    let dir = temp_dir();
    let store = load(&dir);
    let server = FakeServer::start(vec![ok("\"e1\"", &table(1, vec![phrase("a", "aa", "x")]))]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let (sink, events) = CloudSink::detached();
    store.lock().set_cloud_sink(sink);
    run(&store, &hub, Trigger::Manual);
    let start = events.recv_timeout(Duration::from_secs(5)).unwrap();
    let end = events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(start.status.syncing && start.rules.is_empty());
    assert!(!end.status.syncing);
    assert_eq!(end.status.revision, Some(1));
    assert_eq!(end.rules.len(), 1);
    // The payload is the contract's shape.
    let payload = serde_json::to_value(&end).unwrap();
    let keys = |value: &Value| {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(keys(&payload), ["rules", "settings", "status"]);
    assert_eq!(
        keys(&payload["settings"]),
        ["auto_sync_minutes", "base_url", "enabled", "has_key"]
    );
    assert_eq!(
        keys(&payload["status"]),
        [
            "last_attempt_at",
            "last_error",
            "last_success_at",
            "revision",
            "syncing",
            "unreadable_count",
            "updated_at"
        ]
    );
    assert_eq!(
        keys(&payload["rules"][0]),
        ["disabled_local", "id", "rule", "status"]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// The fake server answers the download path that [`endpoint`] builds.
#[test]
fn the_fake_server_sees_the_download_path() {
    let server = FakeServer::start(vec![Reply::now(http("204 No Content", &[], b""))]);
    let url = endpoint(&server.base_url()).unwrap();
    let fetched = tauri::async_runtime::block_on(super::fetch::fetch(
        &url,
        "k",
        None,
        &FetchLimits::default(),
    ));
    assert!(fetched.is_ok());
    assert!(server.requests()[0].starts_with("GET /integrations/quick-text/v1/phrase-table "));
}

/// Review item 3: after a switch from server A to server B, A's phrases never run again:
/// not after the switch, not after a restart while B fails.
#[test]
fn switching_servers_never_brings_the_old_table_back() {
    let dir = temp_dir();
    let store = load(&dir);
    let a = FakeServer::start(vec![ok("\"a\"", &table(1, vec![phrase("a", "aa", "x")]))]);
    let b = FakeServer::start(vec![
        Reply::now(http("500 Internal Server Error", &[], b"")),
        Reply::now(http("500 Internal Server Error", &[], b"")),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&a.base_url(), Some("k"))).unwrap();
    run(&store, &hub, Trigger::Manual);
    assert_eq!(engine_ids(&store), ["cloud:a"]);
    let state = apply_settings(&store, &hub, settings(&b.base_url(), None)).unwrap();
    assert!(state.rules.is_empty() && state.status.last_error.is_none());
    assert!(engine_ids(&store).is_empty());
    assert!(!dir.join("cloud-phrases.json").exists());
    let state = run(&store, &hub, Trigger::Settings);
    assert_eq!(
        state.status.last_error.as_ref().map(|e| e.code),
        Some(code::HTTP_STATUS)
    );
    assert!(engine_ids(&store).is_empty());
    drop(store);
    let store = load(&dir);
    assert!(engine_ids(&store).is_empty());
    let state = run(&store, &hub, Trigger::Startup);
    assert_eq!(
        state.status.last_error.as_ref().map(|e| e.code),
        Some(code::HTTP_STATUS)
    );
    assert!(state.rules.is_empty());
    assert!(engine_ids(&store).is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Review item 3: a cache file of another address (left by a removal that failed, or
/// written before caches recorded their source) is not loaded at startup and is removed.
/// (Each case in its own directory: a third unclean start within minutes would be a
/// crash loop.)
#[test]
fn a_cache_of_another_address_is_not_loaded() {
    // `cache` is written for the store's next start, whose address is B.
    fn start_with(cache: impl FnOnce(&std::path::Path)) -> (PathBuf, Managed) {
        let dir = temp_dir();
        let store = load(&dir);
        apply_settings(&store, &hub(), settings("https://b.example.com", Some("k"))).unwrap();
        drop(store);
        cache(&dir);
        let store = load(&dir);
        (dir, store)
    }
    let (dir, store) = start_with(|dir| {
        write_cache(
            dir,
            "https://a.example.com",
            1,
            vec![phrase("a", "aa", "x")],
        );
    });
    assert!(store.lock().cloud.cache.is_none());
    assert!(engine_ids(&store).is_empty());
    assert_eq!(store.lock().cloud_state().status.last_error, None);
    assert!(!dir.join("cloud-phrases.json").exists());
    std::fs::remove_dir_all(dir).unwrap();
    // The same address, written differently, is the same source.
    let (dir, store) = start_with(|dir| {
        write_cache(
            dir,
            "HTTPS://B.example.com/",
            2,
            vec![phrase("b", "bb", "x")],
        );
    });
    assert_eq!(engine_ids(&store), ["cloud:b"]);
    std::fs::remove_dir_all(dir).unwrap();
    // A file without a recorded source matches no address.
    let (dir, store) = start_with(|dir| {
        write_cache(
            dir,
            "https://b.example.com",
            2,
            vec![phrase("b", "bb", "x")],
        );
        let path = dir.join("cloud-phrases.json");
        let mut legacy: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("source");
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    });
    assert!(engine_ids(&store).is_empty());
    assert!(!dir.join("cloud-phrases.json").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Review item 3: when the old cache cannot be removed, the user is told (`io`, in
/// Chinese) and its phrases still never run, now or after a restart.
#[test]
fn a_cache_that_cannot_be_removed_is_reported_and_stays_unused() {
    let dir = temp_dir();
    let store = load(&dir);
    let a = FakeServer::start(vec![ok("\"a\"", &table(1, vec![phrase("a", "aa", "x")]))]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&a.base_url(), Some("k"))).unwrap();
    run(&store, &hub, Trigger::Manual);
    assert_eq!(engine_ids(&store), ["cloud:a"]);
    // Something holds the name: a directory there cannot be removed as a file.
    std::fs::remove_file(dir.join("cloud-phrases.json")).unwrap();
    std::fs::create_dir_all(dir.join("cloud-phrases.json").join("held")).unwrap();
    let state = apply_settings(&store, &hub, settings("https://b.example.com", None)).unwrap();
    let error = state.status.last_error.clone().unwrap();
    assert_eq!(error.code, code::IO);
    assert_eq!(error.message, cache::NOT_REMOVED);
    assert!(state.rules.is_empty());
    assert!(engine_ids(&store).is_empty());
    drop(store);
    let store = load(&dir);
    assert!(engine_ids(&store).is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Review item 3, at startup: a cache of another address that cannot be removed is
/// reported and not loaded.
#[cfg(unix)]
#[test]
fn a_foreign_cache_that_cannot_be_removed_at_startup_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir();
    let store = load(&dir);
    apply_settings(&store, &hub(), settings("https://b.example.com", Some("k"))).unwrap();
    drop(store);
    write_cache(
        &dir,
        "https://a.example.com",
        1,
        vec![phrase("a", "aa", "x")],
    );
    let mut store = Store::load(dir.clone());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    init(&mut store);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = Managed::new(store);
    assert!(dir.join("cloud-phrases.json").exists(), "removal refused");
    assert!(engine_ids(&store).is_empty());
    let state = store.lock().cloud_state();
    let error = state.status.last_error.unwrap();
    assert_eq!(
        (error.code, error.message.as_str()),
        (code::IO, cache::NOT_REMOVED)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Review item 2: `config` and `compiled` change together for a reader that holds both
/// (the macOS event tap takes `config`, then `compiled`): a reload never shows it a new
/// trigger with the old rule list or the reverse.
#[test]
fn the_engine_swap_is_atomic_for_a_reader_of_both() {
    let dir = temp_dir();
    let store = load(&dir);
    publish(&store, vec![personal_rule("mine", "pp")]);
    let server = FakeServer::start(vec![ok(
        "\"e\"",
        &table(1, vec![phrase("a", "aa", "x"), phrase("b", "bb", "y")]),
    )]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    run(&store, &hub, Trigger::Manual);
    let engine = store.lock().engine.clone();
    let done = std::sync::atomic::AtomicBool::new(false);
    let seen = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut seen = 0u32;
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                let config = engine.config.lock().unwrap();
                let compiled = engine.compiled.lock().unwrap();
                assert!(compiled.config == *config, "torn engine swap");
                let ids: Vec<_> = compiled.rules.iter().map(|r| &r.rule.id).collect();
                let listed: Vec<_> = config.rules.iter().map(|r| &r.id).collect();
                assert_eq!(ids, listed);
                seen += 1;
            }
            seen
        });
        for round in 0..300 {
            let mut s = store.lock();
            s.cloud.prefs.disabled_ids = if round % 2 == 0 {
                vec!["a".into()]
            } else {
                vec![]
            };
            s.apply_engine();
        }
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        reader.join().unwrap()
    });
    assert!(seen > 0);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Review round 2, item 1: a team phrase reload never touches what a run in progress
/// checks. The executors stop a run (a personal one included) at the next erased or
/// typed character when the generation moves or `cancel` is set, so neither may change:
/// a candidate matched before the reload runs the rule it matched (it carries a clone of
/// it, taken from one consistent snapshot, see `the_engine_swap_is_atomic_for_a_reader_of_both`).
/// A publish of the personal configuration still moves the generation, as before.
#[test]
fn a_team_phrase_reload_does_not_interrupt_runs() {
    let dir = temp_dir();
    let store = load(&dir);
    publish(&store, vec![personal_rule("mine", "pp")]);
    let server = FakeServer::start(vec![
        ok("\"e1\"", &table(1, vec![phrase("a", "aa", "x")])),
        Reply::now(http("304 Not Modified", &[], b"")),
        ok("\"e2\"", &table(2, vec![phrase("a", "aa", "new body")])),
    ]);
    let hub = hub();
    let engine = store.lock().engine.clone();
    let ordering = std::sync::atomic::Ordering::SeqCst;
    // A personal run is under way: matched at this generation, not cancelled.
    engine.cancel.store(false, ordering);
    let candidate = engine.generation.load(ordering);
    let still_running = || {
        assert_eq!(engine.generation.load(ordering), candidate, "run stopped");
        assert!(!engine.cancel.load(ordering), "run cancelled");
    };
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    still_running();
    run(&store, &hub, Trigger::Manual);
    assert_eq!(engine_ids(&store), ["mine", "cloud:a"]);
    still_running();
    set_rules_disabled(&store, &hub, vec!["a".into()], true).unwrap();
    set_rules_disabled(&store, &hub, vec!["a".into()], false).unwrap();
    still_running();
    run(&store, &hub, Trigger::Manual);
    run(&store, &hub, Trigger::Manual);
    assert_eq!(
        store.lock().cloud.cache.as_ref().unwrap().table.revision,
        2,
        "a new body for the same trigger was installed"
    );
    still_running();
    // The personal rule set is unaffected by team phrases.
    let ids = store.lock().personal_rule_ids();
    assert!(ids.contains("mine") && !ids.contains("cloud:a"));
    // A publish still moves the generation (and cancels), as it always did.
    publish(
        &store,
        vec![personal_rule("mine", "pp"), personal_rule("two", "tt")],
    );
    assert_ne!(engine.generation.load(ordering), candidate);
    assert!(store.lock().personal_rule_ids().contains("two"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Review round 3: a table over 2000 entries is refused before the cache is written or
/// anything compiled; the old cache and the engine stay. Phrases over the per-phrase
/// limits never reach the engine.
#[test]
fn a_table_over_the_limits_never_reaches_the_cache_or_the_engine() {
    let dir = temp_dir();
    let store = load(&dir);
    let mut regex = phrase("regex", "[a-z]{10000}", "x");
    regex["match"]["kind"] = json!("regex");
    let long_trigger = phrase("long", &"t".repeat(65), "x");
    let long_body = phrase("body", "bd", &"x".repeat(20_001));
    let hostile: Vec<Value> = (0..2001)
        .map(|i| {
            let mut rule = phrase(&format!("h{i}"), "[a-z]{10000}", "x");
            rule["match"]["kind"] = json!("regex");
            rule
        })
        .collect();
    let server = FakeServer::start(vec![
        ok(
            "\"e1\"",
            &table(
                1,
                vec![phrase("a", "aa", "x"), regex, long_trigger, long_body],
            ),
        ),
        ok("\"e2\"", &table(2, hostile)),
    ]);
    let hub = CloudSync::with_limits(FetchLimits {
        timeout: Duration::from_secs(5),
        max_bytes: 2 * 1024 * 1024,
    });
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(
        statuses(&state),
        [
            ("a".to_string(), CloudRuleStatus::Active),
            ("regex".to_string(), CloudRuleStatus::Invalid),
            ("long".to_string(), CloudRuleStatus::Invalid),
            ("body".to_string(), CloudRuleStatus::Invalid),
        ]
    );
    assert_eq!(compiled_ids(&store), ["cloud:a"]);
    let state = run(&store, &hub, Trigger::Manual);
    let error = state.status.last_error.clone().unwrap();
    assert_eq!(error.code, code::TOO_MANY_RULES);
    assert_eq!(
        error.message,
        "团队话术表有 2001 条，超过 2000 条上限，未更新；请联系管理员精简话术表"
    );
    assert_eq!(state.status.revision, Some(1));
    assert_eq!(compiled_ids(&store), ["cloud:a"]);
    let cached: Value =
        serde_json::from_slice(&std::fs::read(dir.join("cloud-phrases.json")).unwrap()).unwrap();
    assert_eq!(cached["table"]["revision"], 1);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Review round 3: a cache over 2000 entries (written by an older release) is not loaded
/// at startup.
#[test]
fn an_oversized_cache_is_not_loaded_at_startup() {
    let dir = temp_dir();
    let store = load(&dir);
    apply_settings(&store, &hub(), settings("https://b.example.com", Some("k"))).unwrap();
    drop(store);
    let rules: Vec<Value> = (0..2001)
        .map(|i| phrase(&format!("p{i}"), &format!("t{i}"), "x"))
        .collect();
    write_cache_unchecked(&dir, "https://b.example.com", table(1, rules));
    let store = load(&dir);
    assert!(store.lock().cloud.cache.is_none());
    assert!(engine_ids(&store).is_empty());
    let error = store.lock().cloud_state().status.last_error.unwrap();
    assert_eq!(error.code, code::TOO_MANY_RULES);
    assert_eq!(
        error.message,
        "本机缓存的团队话术超过 2000 条上限，未使用；将在下次同步成功后恢复"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage one";
const PNG_TWO: &[u8] = b"\x89PNG\r\n\x1a\nimage two";

fn image_phrase(id: &str, trigger: &str, asset: &str) -> Value {
    let mut rule = phrase(id, trigger, "x");
    rule["action"] = json!({ "type": "image", "asset": asset, "name": "码.png" });
    rule
}

fn image_reply(bytes: &[u8]) -> Reply {
    Reply::now(http("200 OK", &[("Content-Type", "image/png")], bytes))
}

/// docs/image-and-zip-package.md §5: after a 200 the missing image is fetched with the key from
/// the sibling path and the phrase runs, in the same sync; a second sync asks for nothing more.
#[test]
fn a_sync_downloads_the_images_of_the_table() {
    let dir = temp_dir();
    let store = load(&dir);
    let asset = asset_name_for(PNG).unwrap();
    let server = FakeServer::start(vec![
        ok("\"e\"", &table(1, vec![image_phrase("img", "qr", &asset)])),
        image_reply(PNG),
        Reply::now(http("304 Not Modified", &[], b"")),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("qtk_s"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(state.status.last_error, None, "{:?}", state.status);
    assert_eq!(
        statuses(&state),
        [("img".to_string(), CloudRuleStatus::Active)]
    );
    assert_eq!(engine_ids(&store), ["cloud:img"]);
    assert_eq!(
        std::fs::read(crate::assets::dir(&dir).join(&asset)).unwrap(),
        PNG
    );
    let requests = server.requests();
    let head = requests[1].to_ascii_lowercase();
    assert!(
        head.starts_with(&format!(
            "get /integrations/quick-text/v1/phrase-assets/{asset} http/1.1"
        )),
        "{head}"
    );
    assert!(head.contains("authorization: bearer qtk_s"));
    run(&store, &hub, Trigger::Manual);
    assert_eq!(server.requests().len(), 3, "the image is not fetched again");
    std::fs::remove_dir_all(dir).unwrap();
}

/// §5: a 304 still fills in what is missing (the earlier try failed), and the engine is reloaded
/// though the table did not change. The failure was `asset_download`, on this phrase only.
#[test]
fn a_304_downloads_a_missing_image_and_reloads_the_engine() {
    let dir = temp_dir();
    let store = load(&dir);
    let asset = asset_name_for(PNG).unwrap();
    let server = FakeServer::start(vec![
        ok(
            "\"e\"",
            &table(
                1,
                vec![image_phrase("img", "qr", &asset), phrase("a", "aa", "x")],
            ),
        ),
        Reply::now(http("404 Not Found", &[], b"{}")),
        Reply::now(http("304 Not Modified", &[], b"")),
        image_reply(PNG),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    let error = state.status.last_error.clone().unwrap();
    assert_eq!(error.code, code::ASSET_DOWNLOAD);
    assert!(state.status.last_success_at.is_some(), "the table is fine");
    assert_eq!(state.status.revision, Some(1));
    assert_eq!(state.rules[0].status, CloudRuleStatus::Invalid);
    assert_eq!(
        state.rules[0].problem.as_deref(),
        Some("图片尚未下载，将在下次同步时重试")
    );
    assert_eq!(state.rules[1].status, CloudRuleStatus::Active);
    assert_eq!(engine_ids(&store), ["cloud:a"]);
    // The serialized state does not carry the problem codes, only the text.
    assert!(!serde_json::to_string(&state.rules[0])
        .unwrap()
        .contains("asset_missing"));

    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(state.status.last_error, None);
    assert_eq!(state.rules[0].status, CloudRuleStatus::Active);
    assert_eq!(engine_ids(&store), ["cloud:img", "cloud:a"]);
    assert!(crate::assets::contains(&dir, &asset));
    let requests = server.requests();
    assert!(requests[2].to_ascii_lowercase().contains("if-none-match"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// §5: what comes back is checked against the name; a failure does not stop the next image.
#[test]
fn a_download_that_does_not_fit_its_name_is_not_stored() {
    let dir = temp_dir();
    let store = load(&dir);
    let good = asset_name_for(PNG_TWO).unwrap();
    let bad = asset_name_for(PNG).unwrap();
    // Both answers are the good image, so it only fits one of the two names, in either order.
    let server = FakeServer::start(vec![
        ok(
            "\"e\"",
            &table(
                1,
                vec![
                    image_phrase("bad", "b1", &bad),
                    image_phrase("good", "g1", &good),
                ],
            ),
        ),
        image_reply(PNG_TWO),
        image_reply(PNG_TWO),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(
        state.status.last_error.as_ref().map(|e| e.code),
        Some(code::ASSET_DOWNLOAD)
    );
    assert!(!crate::assets::contains(&dir, &bad), "wrong bytes refused");
    assert!(crate::assets::contains(&dir, &good));
    assert_eq!(
        statuses(&state),
        [
            ("bad".to_string(), CloudRuleStatus::Invalid),
            ("good".to_string(), CloudRuleStatus::Active)
        ]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// §5: the size limit counts, and a key the server refuses is reported as such and stops the
/// rest; a redirect is not followed.
#[test]
fn asset_downloads_follow_the_rules_of_the_table() {
    let dir = temp_dir();
    let store = load(&dir);
    let names: Vec<String> = [PNG, PNG_TWO, b"\x89PNG\r\n\x1a\nimage 3".as_slice()]
        .iter()
        .map(|bytes| asset_name_for(bytes).unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    let mut big = PNG.to_vec();
    big.resize(MAX_ASSET_BYTES as usize + 1, 0);
    let server = FakeServer::start(vec![
        ok(
            "\"e\"",
            &table(
                1,
                names
                    .iter()
                    .enumerate()
                    .map(|(i, asset)| image_phrase(&format!("p{i}"), &format!("t{i}"), asset))
                    .collect(),
            ),
        ),
        Reply::now(http(
            "302 Found",
            &[("Location", "http://example.invalid/")],
            b"",
        )),
        image_reply(&big),
        Reply::now(http("401 Unauthorized", &[], b"")),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(server.requests().len(), 4);
    // The first failure is what `last_error` says.
    assert_eq!(
        state.status.last_error.as_ref().map(|e| e.code),
        Some(code::ASSET_DOWNLOAD)
    );
    assert!(sorted
        .iter()
        .all(|asset| !crate::assets::contains(&dir, asset)));
    assert!(state
        .rules
        .iter()
        .all(|rule| rule.status == CloudRuleStatus::Invalid));
    std::fs::remove_dir_all(dir).unwrap();
}

/// A key the server refuses while fetching images is `unauthorized`, as for the table.
#[test]
fn a_refused_key_stops_the_image_downloads() {
    let dir = temp_dir();
    let store = load(&dir);
    let (one, two) = (
        asset_name_for(PNG).unwrap(),
        asset_name_for(PNG_TWO).unwrap(),
    );
    let server = FakeServer::start(vec![
        ok(
            "\"e\"",
            &table(
                1,
                vec![image_phrase("a", "aa", &one), image_phrase("b", "bb", &two)],
            ),
        ),
        Reply::now(http("401 Unauthorized", &[], b"")),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(
        server.requests().len(),
        2,
        "the second image is not asked for"
    );
    assert_eq!(
        state.status.last_error.as_ref().map(|e| e.code),
        Some(code::UNAUTHORIZED)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A table cannot name a file outside the library, nor have it requested.
#[test]
fn an_asset_name_outside_the_pattern_is_never_requested() {
    let dir = temp_dir();
    let store = load(&dir);
    let server = FakeServer::start(vec![ok(
        "\"e\"",
        &table(1, vec![image_phrase("evil", "ee", "../../active.json")]),
    )]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(server.requests().len(), 1);
    assert_eq!(state.status.last_error, None);
    assert_eq!(state.rules[0].status, CloudRuleStatus::Invalid);
    assert_eq!(state.rules[0].problem.as_deref(), Some("图片资源名无效"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// §4: opening a program runs through the sync; an action type outside the allowlist stays out of
/// the engine and the text says where else it can go.
#[test]
fn only_allowed_automation_from_the_server_reaches_the_engine() {
    let dir = temp_dir();
    let store = load(&dir);
    let mut common = phrase("common", "cc", "x");
    common["action"] = json!({ "type": "common", "function": "copy" });
    let mut open = phrase("open", "oo", "x");
    open["action"] = json!({ "type": "open", "target": "https://example.com" });
    let mut html = phrase("html", "hh", "x");
    html["action"] = json!({ "type": "html", "html": "<b>x</b>", "plain_text": "x" });
    let mut hotkey = phrase("hotkey", "kk", "x");
    hotkey["action"] = json!({ "type": "hotkey", "keys": [{ "type": "named", "name": "CTRL" }, { "type": "named", "name": "A" }] });
    let server = FakeServer::start(vec![ok(
        "\"e\"",
        &table(1, vec![common, open, html, hotkey]),
    )]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(state.rules[0].status, CloudRuleStatus::Invalid);
    assert_eq!(
        state.rules[0].problem.as_deref(),
        Some("这类操作不通过团队话术同步运行；可导出配置包后导入「我的指令」，审阅后启用")
    );
    assert_eq!(
        engine_ids(&store),
        ["cloud:open", "cloud:html", "cloud:hotkey"]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Safe mode: images are still fetched, nothing runs.
#[test]
fn in_safe_mode_images_download_and_nothing_runs() {
    let dir = temp_dir();
    std::fs::write(dir.join("active.json"), b"{").unwrap();
    let store = load(&dir);
    let asset = asset_name_for(PNG).unwrap();
    let server = FakeServer::start(vec![
        ok("\"e\"", &table(1, vec![image_phrase("img", "qr", &asset)])),
        image_reply(PNG),
    ]);
    let hub = hub();
    apply_settings(&store, &hub, settings(&server.base_url(), Some("k"))).unwrap();
    let state = run(&store, &hub, Trigger::Manual);
    assert_eq!(state.rules[0].status, CloudRuleStatus::Active);
    assert!(crate::assets::contains(&dir, &asset));
    assert!(engine_is_personal(&store));
    assert!(engine_ids(&store).is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

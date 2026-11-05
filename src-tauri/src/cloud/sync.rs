//! When and how a sync runs (docs/cloud-phrases.md §3), and the settings commands that
//! change what it does.
//!
//! - One sync at a time ([`CloudSync`]'s async gate), and single-flight: a request made
//!   while a sync runs waits for it and returns its result instead of running a second
//!   one (so "save settings" plus "sync now" is one download). The exception is a sync
//!   the settings asked for: it never joins one that started before the change, whose
//!   result belongs to the old settings and is dropped. A joined sync must also have run
//!   with the settings still in effect, otherwise the request runs its own.
//! - Order: at startup the cache is loaded ([`init`]) before the engine starts; the
//!   first download runs in the background ([`start`]); then every
//!   `auto_sync_minutes` after the last one (0: no timer); on demand
//!   (`sync_cloud_now`); and once after the settings change.
//! - The download runs without any lock. Its result is installed under the settings
//!   lock: a settings change in between raised the epoch and the result is dropped
//!   (it belongs to the old address or key); the change already queued a new sync.
//! - 200: format checked, cache written, engine reloaded. 304: `last_success_at` only.
//!   After either, images the table needs and the library lacks are downloaded
//!   (docs/image-and-zip-package.md §5) before the result is installed, so a new table's image
//!   phrases do not flicker through `invalid`. A failed image only keeps its own phrase out.
//!   Failure: cache and engine stay as they were, `last_error` says why. Each step is a
//!   `cloud-state` event (start and end of a sync, settings, local disable).
//! - Safe mode is left alone: the store merges into the engine only outside it.
use super::{
    cache, cache_source, code,
    fetch::{self, FetchLimits, Fetched},
    now_rfc3339, CloudCache, CloudSettingsInput, CloudState, SyncError, SyncStatus,
};
use crate::{
    app_state::Managed,
    assets,
    error::CommandError,
    store::{
        prefs::{self, CloudPrefs, AUTO_SYNC_MINUTES},
        Store,
    },
};
use parking_lot::{Condvar, Mutex};
use quicker_domain::{
    cloud_image_assets, cloud_table_from_value, strip_utf8_bom, verify_asset, CloudRuleStatus,
    CloudTable, CloudTableError, Diagnostic, SourceSpan, ASSET_MISSING, CLOUD_MAX_RULES,
};
use reqwest::Url;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager};

/// What asked for a sync (logged; automatic ones stay silent while the subscription
/// is off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Startup,
    Timer,
    Manual,
    Settings,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Timer => "timer",
            Self::Manual => "manual",
            Self::Settings => "settings",
        }
    }
    fn automatic(self) -> bool {
        matches!(self, Self::Startup | Self::Timer)
    }
}

/// Tauri-managed: the one-at-a-time gate, the settings lock and the timer.
pub struct CloudSync {
    /// Held for a whole sync, across the download.
    pub(super) gate: tauri::async_runtime::Mutex<()>,
    /// Syncs started so far; a sync's number is this after its start.
    started: AtomicU64,
    /// Syncs finished so far: fewer than `started` while one runs.
    finished: AtomicU64,
    /// The settings epoch the last finished sync ran with.
    last_epoch: AtomicU64,
    /// Orders settings changes (prefs write, epoch, cache removal) against installing
    /// a download (epoch check, cache write). Taken before the store lock.
    settings: Mutex<()>,
    /// When the timer next fires; `None`: not scheduled. A leaf lock.
    next: Mutex<Option<Instant>>,
    wake: Condvar,
    limits: FetchLimits,
}

impl Default for CloudSync {
    fn default() -> Self {
        Self::with_limits(FetchLimits::default())
    }
}

impl CloudSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: FetchLimits) -> Self {
        Self {
            gate: tauri::async_runtime::Mutex::new(()),
            started: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            last_epoch: AtomicU64::new(0),
            settings: Mutex::new(()),
            next: Mutex::new(None),
            wake: Condvar::new(),
            limits,
        }
    }

    /// The next timer sync `auto_sync_minutes` from now; none while the subscription is
    /// off or the interval is 0.
    fn schedule(&self, prefs: &CloudPrefs) {
        let next = (prefs.enabled && prefs.auto_sync_minutes > 0)
            .then(|| Instant::now() + Duration::from_secs(u64::from(prefs.auto_sync_minutes) * 60));
        *self.next.lock() = next;
        self.wake.notify_all();
    }

    /// Blocks until the timer is due, then clears it (the sync schedules the next one).
    fn wait_due(&self) {
        let mut next = self.next.lock();
        loop {
            match *next {
                None => self.wake.wait(&mut next),
                Some(at) if Instant::now() >= at => {
                    *next = None;
                    return;
                }
                Some(at) => {
                    self.wake.wait_until(&mut next, at);
                }
            }
        }
    }

    /// When the timer fires next (tests).
    #[cfg(test)]
    pub(crate) fn next_due(&self) -> Option<Instant> {
        *self.next.lock()
    }
}

/// Startup, before the engine starts: reads `app.cloud` and the cache and loads the
/// engine with them, so team phrases work offline from the first keystroke. Prefs that
/// cannot be read leave the subscription off for this run and the cache untouched
/// (logged); a cache that cannot be read is `last_error` (`io`, or the table's own
/// code). A cache of another server address than the one set (or one that records
/// none) is never loaded and is removed; if it cannot be removed, `last_error` says so
/// (`io`).
pub fn init(store: &mut Store) {
    let dir = store.dir().to_path_buf();
    match prefs::load_prefs(&dir) {
        Ok(prefs) => store.cloud.prefs = prefs.app.cloud.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(
                event = "cloud.prefs_unavailable",
                error = %e,
                "prefs unavailable; team phrase sync stays off for this run"
            );
            store.apply_engine();
            return;
        }
    }
    match cache::read(&dir) {
        Ok(Some(cache))
            if Some(&cache.source) == cache_source(&store.cloud.prefs.base_url).as_ref() =>
        {
            store.cloud.status.last_success_at = Some(cache.synced_at.clone());
            store.cloud.cache = Some(cache);
        }
        Ok(Some(_)) => remove_foreign_cache(store, &dir),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                event = "cloud.cache_unreadable",
                code = e.code,
                error = %e.detail,
                "cached team phrases not loaded"
            );
            store.cloud.status.last_error = Some(e);
        }
    }
    store.apply_engine();
    let (rules, active) = counts(store);
    tracing::info!(
        event = "cloud.loaded",
        enabled = store.cloud.prefs.enabled,
        revision = store.cloud.cache.as_ref().map(|cache| cache.table.revision),
        rules,
        active,
        "team phrases loaded from the cache"
    );
}

/// Removes a cache that belongs to another server address (startup, settings change);
/// a failure is `last_error` (`io`, a message the user can act on). The caller has
/// already dropped it from memory or never loaded it.
fn remove_foreign_cache(store: &mut Store, dir: &Path) {
    match cache::remove(dir) {
        Ok(()) => tracing::info!(
            event = "cloud.cache_discarded",
            "cached team phrases of another server address removed"
        ),
        Err(e) => {
            tracing::warn!(
                event = "cloud.cache_remove_failed",
                error = %e,
                "cache of another server address not removed; it stays unused"
            );
            store.cloud.status.last_error = Some(SyncError::new(
                code::IO,
                cache::NOT_REMOVED,
                format!("{} not removed ({:?})", cache::FILE, e.kind()),
            ));
        }
    }
}

/// Readable team phrases and how many of them run.
fn counts(store: &Store) -> (usize, usize) {
    let rules = &store.cloud.rules;
    (
        rules.len(),
        rules
            .iter()
            .filter(|rule| rule.status == CloudRuleStatus::Active)
            .count(),
    )
}

/// After the engine started: the first download in the background, then the timer,
/// on one thread (`qt-cloud-sync`). A sync that panics is dropped and the timer goes on
/// (the panic hook has written the crash record).
pub fn start(app: &AppHandle) {
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("qt-cloud-sync".into())
        .spawn(move || {
            let store = handle.state::<Managed>();
            let hub = handle.state::<CloudSync>();
            let run = |trigger| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    tauri::async_runtime::block_on(sync(&store, &hub, trigger))
                }));
            };
            run(Trigger::Startup);
            loop {
                hub.wait_due();
                run(Trigger::Timer);
            }
        });
    if let Err(e) = spawned {
        tracing::error!(
            event = "cloud.scheduler_start_failed",
            error = %e,
            "team phrase sync thread did not start; only manual syncs run"
        );
    }
}

/// One sync, or the result of the one that was running (or started) when this was
/// asked for (see the module documentation). Never fails: the outcome is in `status`.
/// Returns the state after it.
pub async fn sync(store: &Managed, hub: &CloudSync, trigger: Trigger) -> CloudState {
    let started = hub.started.load(Ordering::SeqCst);
    let running = hub.finished.load(Ordering::SeqCst) < started;
    // The syncs numbered above `joinable` answer this request.
    let joinable = if running && trigger != Trigger::Settings {
        started - 1
    } else {
        started
    };
    let _gate = hub.gate.lock().await;
    if hub.started.load(Ordering::SeqCst) > joinable {
        let s = store.lock();
        if hub.last_epoch.load(Ordering::SeqCst) == s.cloud.epoch {
            return s.cloud_state();
        }
    }
    hub.started.fetch_add(1, Ordering::SeqCst);
    let (state, epoch) = run(store, hub, trigger).await;
    hub.last_epoch.store(epoch, Ordering::SeqCst);
    hub.finished.fetch_add(1, Ordering::SeqCst);
    state
}

/// The sync itself, under the gate; also returns the settings epoch it ran with.
async fn run(store: &Managed, hub: &CloudSync, trigger: Trigger) -> (CloudState, u64) {
    let begun = Instant::now();
    let plan = match prepare(store, trigger) {
        Prepared::Skip(epoch) => return (store.lock().cloud_state(), epoch),
        Prepared::Failed(prefs, epoch) => {
            hub.schedule(&prefs);
            return (store.lock().cloud_state(), epoch);
        }
        Prepared::Run(plan) => plan,
    };
    let fetched = fetch::fetch(&plan.url, &plan.key, plan.etag.as_deref(), &hub.limits).await;
    let outcome = match fetched {
        Ok(Fetched::Modified { body, etag }) => {
            parse(&body).map(|(raw, table)| Downloaded::New { raw, table, etag })
        }
        Ok(Fetched::NotModified) => Ok(Downloaded::Same),
        Err(e) => Err(e),
    };
    let wanted = match &outcome {
        Ok(Downloaded::New { table, .. }) => cloud_image_assets(table),
        Ok(Downloaded::Same) => {
            let s = store.lock();
            s.cloud
                .cache
                .as_ref()
                .map(|cache| cloud_image_assets(&cache.table))
                .unwrap_or_default()
        }
        Err(_) => BTreeSet::new(),
    };
    let assets = download_assets(store, hub, &plan, wanted).await;
    (
        finish(store, hub, &plan, outcome, assets, trigger, begun),
        plan.epoch,
    )
}

/// What a sync's image downloads came to.
#[derive(Default)]
struct AssetSync {
    arrived: usize,
    /// The first failure, `last_error` of the sync.
    error: Option<SyncError>,
}

/// After this many failed connections in a row the rest waits for the next sync: the server is
/// gone, and each attempt could cost the whole time budget while the sync holds its gate.
const MAX_ASSET_CONNECTION_FAILURES: usize = 3;

/// Downloads the images of `wanted` the library lacks, one at a time, without any lock. A failure
/// does not stop the others, except a refused key and a server that stopped answering. What
/// arrived stays in the library even if the sync's result is dropped: files are named after their
/// bytes and nothing refers to them by itself. A settings change stops the loop, so the old key
/// is not sent any further.
async fn download_assets(
    store: &Managed,
    hub: &CloudSync,
    plan: &Plan,
    wanted: BTreeSet<String>,
) -> AssetSync {
    let mut result = AssetSync::default();
    let mut connection_failures = 0;
    for asset in wanted {
        if assets::contains(&plan.dir, &asset) {
            continue;
        }
        if store.lock().cloud.epoch != plan.epoch {
            break;
        }
        let fetched = fetch::fetch_asset(&plan.url, &plan.key, &asset, &hub.limits).await;
        let stored = fetched.and_then(|bytes| store_download(&plan.dir, &asset, &bytes));
        let Err(e) = stored else {
            result.arrived += 1;
            connection_failures = 0;
            continue;
        };
        tracing::warn!(
            event = "cloud.asset_download_failed",
            asset = %asset,
            code = e.code,
            error = %e.detail,
            "image not downloaded; the next sync retries"
        );
        let unauthorized = e.code == code::UNAUTHORIZED;
        if e.code == code::NETWORK || e.code == code::TIMEOUT {
            connection_failures += 1;
        } else {
            connection_failures = 0;
        }
        if result.error.is_none() {
            result.error = Some(if unauthorized {
                e
            } else {
                SyncError::new(
                    code::ASSET_DOWNLOAD,
                    "部分图片未能下载，相关话术暂不可用，将在下次同步时重试",
                    format!("{asset}: {}: {}", e.code, e.detail),
                )
            });
        }
        if unauthorized || connection_failures >= MAX_ASSET_CONNECTION_FAILURES {
            break;
        }
    }
    result
}

/// Verifies a download against its name (hash, signature, size) and writes it atomically.
fn store_download(dir: &Path, asset: &str, bytes: &[u8]) -> Result<(), SyncError> {
    verify_asset(asset, bytes).map_err(|e| {
        SyncError::new(
            code::ASSET_DOWNLOAD,
            e.message(),
            format!("not stored: {e}"),
        )
    })?;
    assets::write(dir, asset, bytes).map_err(|e| {
        SyncError::new(
            code::ASSET_DOWNLOAD,
            "图片无法写入本机，请检查数据目录是否可写",
            format!("not written ({:?})", e.kind()),
        )
    })
}

/// What a sync downloads with, read under the store lock.
struct Plan {
    url: Url,
    /// The sync key; never logged or put in an error.
    key: String,
    etag: Option<String>,
    epoch: u64,
    dir: PathBuf,
}

enum Prepared {
    /// An automatic sync while the subscription is off: nothing to do.
    Skip(u64),
    /// Not configured or a bad address: recorded as `last_error`.
    Failed(CloudPrefs, u64),
    Run(Plan),
}

enum Downloaded {
    New {
        raw: Value,
        table: CloudTable,
        etag: Option<String>,
    },
    Same,
}

/// Marks the sync as started (`syncing`, `last_attempt_at`, one `cloud-state`) or
/// records why it cannot run.
fn prepare(store: &Managed, trigger: Trigger) -> Prepared {
    let mut s = store.lock();
    let epoch = s.cloud.epoch;
    let prefs = &s.cloud.prefs;
    if trigger.automatic() && (!prefs.enabled || prefs.key.is_empty() || prefs.base_url.is_empty()) {
        return Prepared::Skip(epoch);
    }
    let ready = if !prefs.enabled {
        Err(SyncError::new(
            code::NOT_CONFIGURED,
            "团队话术同步未开启",
            "team phrase sync is off",
        ))
    } else if prefs.key.is_empty() {
        Err(SyncError::new(
            code::NOT_CONFIGURED,
            "未设置同步密钥",
            "no sync key is set",
        ))
    } else {
        fetch::endpoint(&prefs.base_url)
    };
    s.cloud.status.last_attempt_at = Some(now_rfc3339());
    match ready {
        Err(e) => {
            tracing::warn!(
                event = "cloud.sync_failed",
                trigger = trigger.as_str(),
                code = e.code,
                error = %e.detail,
                elapsed_ms = 0u64,
                "team phrases not synced"
            );
            s.cloud.status.last_error = Some(e);
            s.report_cloud();
            Prepared::Failed(s.cloud.prefs.clone(), epoch)
        }
        Ok(url) => {
            s.cloud.status.syncing = true;
            s.report_cloud();
            Prepared::Run(Plan {
                url,
                key: s.cloud.prefs.key.clone(),
                etag: s.cloud.cache.as_ref().and_then(|cache| cache.etag.clone()),
                epoch,
                dir: s.dir().to_path_buf(),
            })
        }
    }
}

const NOT_A_TABLE: &str = "服务器返回的内容不是团队话术表，请检查服务器地址";

/// The body as JSON (kept for the cache) and as a table. The table-level admission
/// (`quicker_domain::CLOUD_MAX_RULES`) happens here, before the cache is written or
/// anything compiled; the per-phrase limits are checked by the merge before compiling.
fn parse(body: &[u8]) -> Result<(Value, CloudTable), SyncError> {
    let raw: Value = serde_json::from_slice(strip_utf8_bom(body)).map_err(|e| {
        SyncError::new(
            code::INVALID_FORMAT,
            NOT_A_TABLE,
            format!(
                "not JSON ({:?} at line {}, column {})",
                e.classify(),
                e.line(),
                e.column()
            ),
        )
    })?;
    let table = cloud_table_from_value(raw.clone()).map_err(|e| {
        let (code, message) = match &e {
            CloudTableError::SchemaNewer { .. } => (
                code::SCHEMA_NEWER,
                "服务器上的团队话术表格式较新，请升级 Quicktext；暂时继续使用已缓存的话术".into(),
            ),
            CloudTableError::Malformed { .. } => (code::INVALID_FORMAT, NOT_A_TABLE.into()),
            CloudTableError::TooManyRules { count } => (
                code::TOO_MANY_RULES,
                format!(
                    "团队话术表有 {count} 条，超过 {CLOUD_MAX_RULES} 条上限，未更新；请联系管理员精简话术表"
                ),
            ),
        };
        SyncError::new(code, message, e.to_string())
    })?;
    Ok((raw, table))
}

/// Installs the outcome (see the module documentation) and schedules the next timer
/// sync.
fn finish(
    store: &Managed,
    hub: &CloudSync,
    plan: &Plan,
    outcome: Result<Downloaded, SyncError>,
    assets: AssetSync,
    trigger: Trigger,
    begun: Instant,
) -> CloudState {
    let _settings = hub.settings.lock();
    let elapsed_ms = begun.elapsed().as_millis() as u64;
    let mut s = store.lock();
    if s.cloud.epoch != plan.epoch {
        s.cloud.status.syncing = false;
        s.report_cloud();
        tracing::info!(
            event = "cloud.sync",
            trigger = trigger.as_str(),
            outcome = "discarded",
            elapsed_ms,
            "settings changed during the download; result dropped"
        );
        return s.cloud_state();
    }
    let now = now_rfc3339();
    let installed = match outcome {
        Ok(Downloaded::New { raw, table, etag }) => {
            // The settings lock keeps the epoch (and the cache file) as checked while
            // the store is free for other commands during the write.
            drop(s);
            let written = cache::write(&plan.dir, plan.url.as_str(), etag.as_deref(), &now, &raw);
            s = store.lock();
            written.map(|()| {
                s.cloud.cache = Some(CloudCache {
                    source: plan.url.to_string(),
                    etag,
                    synced_at: now.clone(),
                    table,
                });
                "updated"
            })
        }
        Ok(Downloaded::Same) => Ok("not_modified"),
        Err(e) => Err(e),
    };
    s.cloud.status.syncing = false;
    match installed {
        Ok(outcome) => {
            s.cloud.status.last_success_at = Some(now);
            s.cloud.status.last_error = assets.error;
            // A phrase still waiting for its image may have got it another way (an import).
            let waiting = s
                .cloud
                .rules
                .iter()
                .any(|rule| rule.problem_codes.iter().any(|code| code == ASSET_MISSING));
            if outcome == "updated" || assets.arrived > 0 || waiting {
                s.apply_engine();
            }
            s.report_cloud();
            let (rules, active) = counts(&s);
            // What the user reads as `problem` is in Chinese; the codes go here.
            let invalid: BTreeSet<&str> = s
                .cloud
                .rules
                .iter()
                .flat_map(|rule| rule.problem_codes.iter().map(String::as_str))
                .collect();
            tracing::info!(
                event = "cloud.sync",
                trigger = trigger.as_str(),
                outcome,
                revision = s.cloud.cache.as_ref().map(|cache| cache.table.revision),
                rules,
                active,
                invalid_codes = ?invalid,
                unreadable = s
                    .cloud
                    .cache
                    .as_ref()
                    .map_or(0, |cache| cache.table.unreadable_count),
                elapsed_ms,
                "team phrases synced"
            );
            let stale = stale_disabled_ids(&s);
            drop(s);
            drop_stale_disabled_ids(store, &plan.dir, stale);
        }
        Err(e) => {
            tracing::warn!(
                event = "cloud.sync_failed",
                trigger = trigger.as_str(),
                code = e.code,
                error = %e.detail,
                elapsed_ms,
                "team phrases not synced; cache and engine unchanged"
            );
            s.cloud.status.last_error = Some(e);
            s.report_cloud();
            drop(s);
        }
    }
    let s = store.lock();
    hub.schedule(&s.cloud.prefs);
    s.cloud_state()
}

/// `disabled_ids` that match no team phrase of the table in use (§2).
fn stale_disabled_ids(s: &Store) -> Vec<String> {
    let Some(cache) = &s.cloud.cache else {
        return vec![];
    };
    let ids: BTreeSet<&str> = cache
        .table
        .rules
        .iter()
        .map(|rule| rule.id.as_str())
        .collect();
    s.cloud
        .prefs
        .disabled_ids
        .iter()
        .filter(|id| !ids.contains(id.as_str()))
        .cloned()
        .collect()
}

/// Removes `stale` from `app.cloud.disabled_ids` after a successful sync. The settings
/// lock is held by the caller. A prefs file that cannot be written keeps them (logged);
/// they match nothing, so nothing else depends on it.
fn drop_stale_disabled_ids(store: &Managed, dir: &Path, stale: Vec<String>) {
    if stale.is_empty() {
        return;
    }
    let written = prefs::update_prefs(dir, |prefs| match prefs.app.cloud.as_mut() {
        Some(cloud) => {
            let before = cloud.disabled_ids.len();
            cloud.disabled_ids.retain(|id| !stale.contains(id));
            cloud.disabled_ids.len() != before
        }
        None => false,
    });
    match written {
        Ok(_) => {
            store
                .lock()
                .cloud
                .prefs
                .disabled_ids
                .retain(|id| !stale.contains(id));
            tracing::info!(
                event = "cloud.disabled_cleaned",
                count = stale.len(),
                "locally disabled ids that match no team phrase were dropped"
            );
        }
        Err(e) => tracing::warn!(
            event = "cloud.disabled_cleanup_failed",
            error = %e,
            "stale locally disabled ids kept"
        ),
    }
}

/// `set_cloud_settings` (§5): checks the settings, writes `app.cloud` and applies it.
/// Allowed in safe mode (prefs are not the configuration safe mode protects). A new
/// server address (compared as [`cache_source`] normalizes it) drops the cache of the
/// old one from memory and disk; should the file stay, the next start ignores it anyway
/// (its recorded source differs). Does not wait for the sync that the caller starts
/// next.
pub fn apply_settings(
    store: &Managed,
    hub: &CloudSync,
    input: CloudSettingsInput,
) -> Result<CloudState, CommandError> {
    let base_url = input.base_url.trim().to_string();
    let key = input.key.as_deref().map(str::trim);
    validate(&base_url, key, input.auto_sync_minutes)?;
    let _settings = hub.settings.lock();
    // The store lock is held only to read the directory, not for the write.
    let dir = store.lock().dir().to_path_buf();
    let mut stored = CloudPrefs::default();
    prefs::update_prefs(&dir, |prefs| {
        let mut cloud = prefs.app.cloud.clone().unwrap_or_default();
        cloud.enabled = input.enabled;
        cloud.base_url.clone_from(&base_url);
        cloud.auto_sync_minutes = input.auto_sync_minutes;
        if let Some(key) = key {
            cloud.key = key.to_string();
        }
        let changed = prefs.app.cloud.as_ref() != Some(&cloud);
        stored = cloud.clone();
        prefs.app.cloud = Some(cloud);
        changed
    })?;
    let (state, source_changed) = {
        let mut s = store.lock();
        let new_source = cache_source(&stored.base_url);
        let source_changed = cache_source(&s.cloud.prefs.base_url) != new_source
            || s.cloud
                .cache
                .as_ref()
                .is_some_and(|cache| Some(&cache.source) != new_source.as_ref());
        s.cloud.epoch += 1;
        s.cloud.prefs = stored.clone();
        if source_changed {
            s.cloud.cache = None;
            s.cloud.status = SyncStatus {
                syncing: s.cloud.status.syncing,
                ..SyncStatus::default()
            };
            remove_foreign_cache(&mut s, &dir);
        }
        s.apply_engine();
        s.report_cloud();
        (s.cloud_state(), source_changed)
    };
    hub.schedule(&stored);
    tracing::info!(
        event = "cloud.settings",
        enabled = stored.enabled,
        has_key = !stored.key.is_empty(),
        auto_sync_minutes = stored.auto_sync_minutes,
        source_changed,
        "team phrase sync settings saved"
    );
    Ok(state)
}

/// `validation` diagnostics at `settings.<field>`; never quotes the key.
fn validate(base_url: &str, key: Option<&str>, minutes: u32) -> Result<(), CommandError> {
    let mut diagnostics = vec![];
    if !base_url.is_empty() {
        if let Err(e) = fetch::endpoint(base_url) {
            diagnostics.push(
                Diagnostic::error(code::INVALID_URL, e.message, SourceSpan::default())
                    .at("settings.base_url"),
            );
        }
    }
    if key.is_some_and(|key| !key.bytes().all(|byte| byte.is_ascii_graphic())) {
        diagnostics.push(
            Diagnostic::error(
                "invalid_key",
                "同步密钥只能包含英文字母、数字和符号，不能有空格或中文",
                SourceSpan::default(),
            )
            .at("settings.key"),
        );
    }
    if !AUTO_SYNC_MINUTES.contains(&minutes) {
        diagnostics.push(
            Diagnostic::error(
                "invalid_interval",
                "自动同步间隔只能是 0（仅手动）、15、30、60、120 或 240 分钟",
                SourceSpan::default(),
            )
            .at("settings.auto_sync_minutes"),
        );
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CommandError::validation(diagnostics))
    }
}

/// `set_cloud_rules_disabled` (§5): switches team phrases off or on for this computer
/// (`app.cloud.disabled_ids`, ids without the `cloud:` prefix) and reloads the engine.
/// Allowed in safe mode, like the settings.
pub fn set_rules_disabled(
    store: &Managed,
    hub: &CloudSync,
    ids: Vec<String>,
    disabled: bool,
) -> Result<CloudState, CommandError> {
    let ids: BTreeSet<String> = ids.into_iter().filter(|id| !id.is_empty()).collect();
    let _settings = hub.settings.lock();
    let dir = store.lock().dir().to_path_buf();
    let mut stored = vec![];
    prefs::update_prefs(&dir, |prefs| {
        let mut cloud = prefs.app.cloud.clone().unwrap_or_default();
        let mut set: BTreeSet<String> = cloud.disabled_ids.iter().cloned().collect();
        for id in &ids {
            if disabled {
                set.insert(id.clone());
            } else {
                set.remove(id);
            }
        }
        cloud.disabled_ids = set.into_iter().collect();
        stored.clone_from(&cloud.disabled_ids);
        let changed = match &prefs.app.cloud {
            Some(before) => before != &cloud,
            None => !cloud.disabled_ids.is_empty(),
        };
        if changed {
            prefs.app.cloud = Some(cloud);
        }
        changed
    })?;
    let mut s = store.lock();
    s.cloud.prefs.disabled_ids = stored;
    s.apply_engine();
    s.report_cloud();
    tracing::info!(
        event = "cloud.rules_disabled",
        count = ids.len(),
        disabled,
        total_disabled = s.cloud.prefs.disabled_ids.len(),
        "team phrases switched on this computer"
    );
    Ok(s.cloud_state())
}

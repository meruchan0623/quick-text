//! Team phrases (「团队话术」, docs/cloud-phrases.md): the read-only phrase table this
//! app downloads from the TSim customer-service system and runs next to the personal
//! rules.
//!
//! - Settings live in `prefs.json` `app.cloud` ([`crate::store::prefs::CloudPrefs`]),
//!   never in exports or the revision history; the sync key is write-only (the frontend
//!   only ever sees `has_key`).
//! - The last good download is `cloud-phrases.json` in the data directory ([`cache`]),
//!   read at startup so team phrases work offline.
//! - Downloads ([`fetch`]) run without the store lock, one at a time ([`sync`]).
//! - The store keeps everything here in [`CloudData`] and loads the engine with the
//!   effective configuration (`Store::apply_engine`, `quicker_domain::merge_cloud`).
//!   A sync never writes `active.json` or `revisions/`.
//!
//! Lock order: [`CloudSync`]'s settings lock, then the store lock (the prefs file lock
//! is taken and released in between, never while the store lock is held here). The
//! scheduler lock is a leaf.
mod cache;
mod fetch;
mod sync;

pub use sync::{apply_settings, init, set_rules_disabled, start, sync, CloudSync, Trigger};

use crate::{events::CloudSink, store::prefs::CloudPrefs, store::Store};
use quicker_domain::{CloudRuleState, CloudTable};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Error codes of `CloudState.status.last_error` (docs/cloud-phrases.md §3).
pub mod code {
    /// Not enabled, or no server address or key.
    pub const NOT_CONFIGURED: &str = "not_configured";
    pub const INVALID_URL: &str = "invalid_url";
    /// HTTP 401 or 403.
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const NETWORK: &str = "network";
    pub const TIMEOUT: &str = "timeout";
    /// Any other status than 2xx, 304, 401 and 403.
    pub const HTTP_STATUS: &str = "http_status";
    pub const TOO_LARGE: &str = "too_large";
    /// More entries than `quicker_domain::CLOUD_MAX_RULES` (2000).
    pub const TOO_MANY_RULES: &str = "too_many_rules";
    pub const INVALID_FORMAT: &str = "invalid_format";
    pub const SCHEMA_NEWER: &str = "schema_newer";
    /// The cache file could not be read or written.
    pub const IO: &str = "io";
    /// An image of the table could not be fetched, verified or stored; its phrase is `invalid`
    /// (`asset_missing`) and the next sync retries.
    pub const ASSET_DOWNLOAD: &str = "asset_download";
}

/// `CloudState.status.last_error`: a code from [`code`] and `message`, the text the
/// frontend shows as is (Chinese, for the user). The technical `detail` (English: the
/// HTTP status, the OS or TLS error, a parser position) goes to the log only. Neither
/// contains the sync key or table content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SyncError {
    pub code: &'static str,
    pub message: String,
    #[serde(skip)]
    pub detail: String,
}

impl SyncError {
    pub fn new(code: &'static str, message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

/// The `CloudState` of the IPC contract (docs/cloud-phrases.md §5): the payload of
/// `cloud-state` and of every cloud command.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CloudState {
    pub settings: CloudSettingsView,
    pub status: CloudStatusView,
    /// Every readable team phrase with its status, in download order; empty while the
    /// subscription is off.
    pub rules: Vec<CloudRuleState>,
}

/// The settings as the frontend sees them: the key only as `has_key`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CloudSettingsView {
    pub enabled: bool,
    pub base_url: String,
    pub has_key: bool,
    pub auto_sync_minutes: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CloudStatusView {
    pub syncing: bool,
    /// RFC 3339 (UTC).
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<SyncError>,
    /// `revision` of the table in use (the cached one); null without one or while the
    /// subscription is off.
    pub revision: Option<u64>,
    pub updated_at: Option<String>,
    pub unreadable_count: usize,
}

/// `set_cloud_settings` `settings` (docs/cloud-phrases.md §5). `key`: absent or null
/// keeps the stored key, `""` clears it.
#[derive(Clone, Deserialize)]
pub struct CloudSettingsInput {
    pub enabled: bool,
    pub base_url: String,
    #[serde(default)]
    pub key: Option<String>,
    pub auto_sync_minutes: u32,
}

/// The key is shown only as whether one was given.
impl fmt::Debug for CloudSettingsInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudSettingsInput")
            .field("enabled", &self.enabled)
            .field("base_url", &self.base_url)
            .field(
                "key",
                &self.key.as_ref().map(|key| {
                    if key.is_empty() {
                        "<clear>"
                    } else {
                        "<redacted>"
                    }
                }),
            )
            .field("auto_sync_minutes", &self.auto_sync_minutes)
            .finish()
    }
}

/// The last good download, as cached in `cloud-phrases.json`.
#[derive(Clone, Debug)]
pub struct CloudCache {
    /// The download URL it came from ([`cache_source`]); empty when the file did not
    /// record one. Used only while it is the current server address's.
    pub source: String,
    /// The `ETag` it came with, sent back as `If-None-Match`.
    pub etag: Option<String>,
    /// When it was downloaded, RFC 3339 (UTC).
    pub synced_at: String,
    pub table: CloudTable,
}

/// What a sync is doing or last did (not persisted; `last_success_at` starts from the
/// cache's `synced_at`).
#[derive(Clone, Debug, Default)]
pub struct SyncStatus {
    pub syncing: bool,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<SyncError>,
}

/// Everything about team phrases that the store holds, under the store lock.
#[derive(Default)]
pub struct CloudData {
    /// `prefs.json` `app.cloud` as last read or written by this process.
    pub prefs: CloudPrefs,
    pub cache: Option<CloudCache>,
    pub status: SyncStatus,
    /// The statuses of the last merge (`Store::apply_engine`).
    pub rules: Vec<CloudRuleState>,
    /// Raised by every settings change: a download that started before it is
    /// discarded instead of installed.
    pub epoch: u64,
    pub(crate) sink: Option<CloudSink>,
}

impl CloudData {
    /// The table the engine merges: the cached one while the subscription is on.
    pub fn table(&self) -> Option<&CloudTable> {
        self.prefs
            .enabled
            .then_some(self.cache.as_ref())
            .flatten()
            .map(|cache| &cache.table)
    }

    /// The disabled ids as a set, for `merge_cloud`.
    pub fn disabled_ids(&self) -> std::collections::BTreeSet<String> {
        self.prefs.disabled_ids.iter().cloned().collect()
    }
}

impl Store {
    /// `get_cloud_state` and the `cloud-state` payload.
    pub fn cloud_state(&self) -> CloudState {
        let cloud = &self.cloud;
        let table = cloud.table();
        CloudState {
            settings: CloudSettingsView {
                enabled: cloud.prefs.enabled,
                base_url: cloud.prefs.base_url.clone(),
                has_key: !cloud.prefs.key.is_empty(),
                auto_sync_minutes: cloud.prefs.auto_sync_minutes,
            },
            status: CloudStatusView {
                syncing: cloud.status.syncing,
                last_attempt_at: cloud.status.last_attempt_at.clone(),
                last_success_at: cloud.status.last_success_at.clone(),
                last_error: cloud.status.last_error.clone(),
                revision: table.map(|table| table.revision),
                updated_at: table.and_then(|table| table.updated_at.clone()),
                unreadable_count: table.map_or(0, |table| table.unreadable_count),
            },
            rules: cloud.rules.clone(),
        }
    }

    /// From now on every change is reported to `sink` as `cloud-state`. Called once,
    /// in setup.
    pub fn set_cloud_sink(&mut self, sink: CloudSink) {
        self.cloud.sink = Some(sink);
    }

    /// Queues the current state as one `cloud-state` event, under the store lock, so
    /// events follow the order of the changes. Nothing without a sink (tests).
    pub fn report_cloud(&self) {
        if let Some(sink) = &self.cloud.sink {
            sink.report(self.cloud_state());
        }
    }
}

/// Which server address a cache belongs to: the download URL for `base_url`, normalized
/// exactly as the download builds it (scheme and host in lower case, trailing `/`
/// removed). `None` for an address that cannot be downloaded from (empty or invalid),
/// which no cache matches.
pub fn cache_source(base_url: &str) -> Option<String> {
    fetch::endpoint(base_url).ok().map(String::from)
}

/// Now as RFC 3339 UTC with seconds (`2026-10-28T08:30:00Z`).
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests;

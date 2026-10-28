//! `cloud-phrases.json` (docs/cloud-phrases.md §2): the last table downloaded with
//! status 200, `{ "source": "...", "etag": "...", "synced_at": "RFC3339", "table": { …download… } }`,
//! written atomically. `source` is the download URL it came from, normalized as the
//! download normalizes it ([`super::cache_source`]); a cache whose source is not the
//! current server address is never used (a file left by a failed removal after a server
//! change cannot come back at the next start). The table is kept as downloaded and read
//! with the same parser as a download, so a restart sees exactly what the sync saw.
use super::{code, CloudCache, SyncError};
use crate::store::atomic_json;
use quicker_domain::{cloud_table_from_value, strip_utf8_bom, CloudTableError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, io, path::Path};

pub const FILE: &str = "cloud-phrases.json";

const UNREADABLE: &str = "本机缓存的团队话术无法读取，将在下次同步成功后恢复";
const NOT_WRITTEN: &str = "团队话术已下载，但无法写入本机缓存，请检查数据目录是否可写";
/// `last_error.message` when a cache of another server address could not be removed.
pub const NOT_REMOVED: &str =
    "旧服务器地址的团队话术缓存（cloud-phrases.json）无法删除，其中的话术不会被使用；请检查数据目录的权限";

#[derive(Deserialize)]
struct CacheFile {
    /// Absent in a file written before sources were recorded: matches no address.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    etag: Option<String>,
    synced_at: String,
    table: Value,
}

#[derive(Serialize)]
struct CacheFileRef<'a> {
    source: &'a str,
    etag: Option<&'a str>,
    synced_at: &'a str,
    table: &'a Value,
}

/// The cached table; `Ok(None)` when there is none. It passes the same admission as a
/// download (the table limit here, the per-phrase limits in the merge), whoever wrote it.
/// A file that cannot be read, is not a cache file or holds a table that does not read is
/// `io`; a table of a newer schema (after a downgrade) is `schema_newer`; one over the
/// rule limit is `too_many_rules`. Messages never quote the file.
pub fn read(dir: &Path) -> Result<Option<CloudCache>, SyncError> {
    let bytes = match fs::read(dir.join(FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(SyncError::new(
                code::IO,
                UNREADABLE,
                format!("{FILE} could not be read ({:?})", e.kind()),
            ))
        }
    };
    let file: CacheFile = serde_json::from_slice(strip_utf8_bom(&bytes)).map_err(|e| {
        SyncError::new(
            code::IO,
            UNREADABLE,
            format!(
                "{FILE} is not a cache file ({:?} at line {}, column {})",
                e.classify(),
                e.line(),
                e.column()
            ),
        )
    })?;
    let table = cloud_table_from_value(file.table).map_err(|e| {
        let (code, message) = match e {
            CloudTableError::SchemaNewer { .. } => (
                code::SCHEMA_NEWER,
                "本机缓存的团队话术来自更新版本的 Quicktext，请升级后重新同步",
            ),
            // Written by an older release or by hand: the same admission as a download.
            CloudTableError::TooManyRules { .. } => (
                code::TOO_MANY_RULES,
                "本机缓存的团队话术超过 2000 条上限，未使用；将在下次同步成功后恢复",
            ),
            CloudTableError::Malformed { .. } => (code::IO, UNREADABLE),
        };
        SyncError::new(code, message, format!("cached table refused: {e}"))
    })?;
    Ok(Some(CloudCache {
        source: file.source.unwrap_or_default(),
        etag: file.etag,
        synced_at: file.synced_at,
        table,
    }))
}

/// Replaces the cache atomically (temp file, fsync, rename).
pub fn write(
    dir: &Path,
    source: &str,
    etag: Option<&str>,
    synced_at: &str,
    table: &Value,
) -> Result<(), SyncError> {
    fs::create_dir_all(dir).map_err(|e| {
        SyncError::new(
            code::IO,
            NOT_WRITTEN,
            format!("data directory not created ({:?})", e.kind()),
        )
    })?;
    atomic_json(
        &dir.join(FILE),
        &CacheFileRef {
            source,
            etag,
            synced_at,
            table,
        },
    )
    .map_err(|e| SyncError::new(code::IO, NOT_WRITTEN, format!("{FILE} not written: {e}")))
}

/// Removes the cache (it belongs to another server address). A missing file is fine.
pub fn remove(dir: &Path) -> io::Result<()> {
    match fs::remove_file(dir.join(FILE)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

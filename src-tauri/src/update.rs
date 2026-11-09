//! In-app update check and download, backed by the GitHub Releases of
//! `meruchan0623/quick-text` (what `.github/workflows/installers.yml` publishes).
//!
//! - Every release tag is `v<SemVer 2.0.0>` (`scripts/version.mjs ci`): plain `vX.Y.Z` is a
//!   release, a tag with a `-` part (`v0.4.0-beta.1`, the CI test builds `v0.3.2-test.17`) a
//!   prerelease. Tags that are not SemVer (the old `test-3-ddff827`) and drafts are ignored.
//! - The newest release is chosen by SemVer precedence ([`select_latest`]), not by date.
//! - Each release carries one package per platform (macOS arm64 `*.dmg`, Windows x64
//!   `*-Windows-x64-Portable.zip`) and `SHA256SUMS-macos.txt` / `SHA256SUMS-windows.txt`, which the
//!   download is checked against ([`expected_sha256`]).
//!
//! Nothing personal is sent: the requests carry only `User-Agent: Quicktext/<version>`. TLS goes
//! through the platform stack (reqwest `native-tls`), as for the team phrases. Redirects are
//! followed only to https (GitHub serves release assets from its object storage).
//!
//! The frontend never hands in a URL: `download_update` takes a tag and resolves its release
//! again here, and only assets under this repository's release download path are fetched.
use crate::error::{CommandError, IoOp};
use reqwest::{
    header::{HeaderValue, ACCEPT},
    StatusCode, Url,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The repository's REST API root.
const API: &str = "https://api.github.com/repos/meruchan0623/quick-text";
/// Release assets of this repository, and nothing else, are downloaded.
const DOWNLOAD_PREFIX: &str = "https://github.com/meruchan0623/quick-text/releases/download/";
/// Frontend event with an [`UpdateProgress`] payload.
pub const PROGRESS_EVENT: &str = "update-download-progress";

/// Budget of one API request (release list, one release, a checksum file).
const API_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A download stalls out when no byte arrives for this long; it has no total limit.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const API_MAX_BYTES: u64 = 8 * 1024 * 1024;
const SUMS_MAX_BYTES: u64 = 64 * 1024;
const ASSET_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// Release notes longer than this are cut (characters).
const NOTES_MAX_CHARS: usize = 20_000;
/// At most this often a progress event (plus the last one).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// `CommandError::Update.kind` values.
pub mod kind {
    pub const NETWORK: &str = "network";
    pub const TIMEOUT: &str = "timeout";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const HTTP_STATUS: &str = "http_status";
    pub const INVALID_RESPONSE: &str = "invalid_response";
    pub const INVALID_TAG: &str = "invalid_tag";
    pub const RELEASE_NOT_FOUND: &str = "release_not_found";
    pub const NO_ASSET: &str = "no_asset";
    pub const TOO_LARGE: &str = "too_large";
    pub const INCOMPLETE: &str = "incomplete";
    pub const CHECKSUM_MISMATCH: &str = "checksum_mismatch";
    pub const BUSY: &str = "busy";
    pub const NOT_DOWNLOADED: &str = "not_downloaded";
}

/// The user-facing error; `detail` (English, technical) goes to the log only.
pub fn fail(
    kind: &'static str,
    message: impl Into<String>,
    detail: impl AsRef<str>,
) -> CommandError {
    tracing::warn!(
        event = "update.failed",
        kind,
        detail = detail.as_ref(),
        "update step failed"
    );
    CommandError::Update {
        kind,
        message: message.into(),
    }
}

// ---- the GitHub REST shapes (only the fields used) ----

#[derive(Clone, Debug, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GhAsset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub browser_download_url: String,
}

// ---- the IPC shapes (src/lib/update.ts) ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateAsset {
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateRelease {
    /// SemVer without the tag's `v`.
    pub version: String,
    pub tag: String,
    pub prerelease: bool,
    pub published_at: Option<String>,
    /// Markdown, as on the release page.
    pub notes: String,
    pub html_url: String,
    /// The package for this platform; `None` when the release has none (or the platform has no
    /// packages at all).
    pub asset: Option<UpdateAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateCheck {
    pub current: String,
    pub latest: Option<UpdateRelease>,
    pub update_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateProgress {
    pub tag: String,
    pub downloaded: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateDownload {
    pub path: String,
    pub sha256: String,
    /// False when the release has no checksum file, or it does not list the package.
    pub verified: bool,
}

// ---- pure rules ----

/// Which package of a release this build installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacosArm64,
    WindowsX64,
    /// No package is published for it.
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Self::MacosArm64
        } else if cfg!(all(windows, target_arch = "x86_64")) {
            Self::WindowsX64
        } else {
            Self::Other
        }
    }

    /// The checksum file the CI writes next to this platform's package.
    pub fn sums_name(self) -> Option<&'static str> {
        match self {
            Self::MacosArm64 => Some("SHA256SUMS-macos.txt"),
            Self::WindowsX64 => Some("SHA256SUMS-windows.txt"),
            Self::Other => None,
        }
    }

    /// 0: not this platform's package; higher is a better match.
    fn score(self, name: &str) -> u8 {
        let lower = name.to_ascii_lowercase();
        match self {
            Self::MacosArm64 if lower.ends_with(".dmg") => {
                if lower.contains("aarch64") || lower.contains("arm64") {
                    2
                } else if lower.contains("x64") || lower.contains("x86_64") {
                    0
                } else {
                    1
                }
            }
            Self::WindowsX64 if lower.ends_with("-windows-x64-portable.zip") => 1,
            _ => 0,
        }
    }
}

/// `v1.2.3` or `1.2.3` as SemVer; `None` for anything else.
pub fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

fn is_prerelease(release: &GhRelease, version: &Version) -> bool {
    !version.pre.is_empty() || release.prerelease
}

/// The newest release by SemVer precedence (build metadata ignored; on a tie the first, i.e. the
/// most recently created, wins). Drafts and non-SemVer tags are skipped; prereleases unless
/// `include_prerelease`.
pub fn select_latest(
    releases: &[GhRelease],
    include_prerelease: bool,
) -> Option<(&GhRelease, Version)> {
    let mut best: Option<(&GhRelease, Version)> = None;
    for release in releases.iter().filter(|r| !r.draft) {
        let Some(version) = parse_tag(&release.tag_name) else {
            continue;
        };
        if !include_prerelease && is_prerelease(release, &version) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(_, b)| version.cmp_precedence(b) == Ordering::Greater)
        {
            best = Some((release, version));
        }
    }
    best
}

/// A name that is a plain file name on every platform (it becomes a file in Downloads).
pub fn is_safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.ends_with(' ')
        && !name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
}

/// An asset this app may download: a safe name, under this repository's release downloads.
fn is_downloadable(asset: &GhAsset) -> bool {
    is_safe_file_name(&asset.name) && asset.browser_download_url.starts_with(DOWNLOAD_PREFIX)
}

/// This platform's package in `release`.
pub fn pick_asset(release: &GhRelease, platform: Platform) -> Option<&GhAsset> {
    let mut best: Option<(&GhAsset, u8)> = None;
    for asset in release.assets.iter().filter(|a| is_downloadable(a)) {
        let score = platform.score(&asset.name);
        if score > 0 && best.is_none_or(|(_, s)| score > s) {
            best = Some((asset, score));
        }
    }
    best.map(|(asset, _)| asset)
}

/// The checksum file for this platform's package in `release`.
pub fn pick_sums(release: &GhRelease, platform: Platform) -> Option<&GhAsset> {
    let name = platform.sums_name()?;
    release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(name) && is_downloadable(a))
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

pub fn to_update_release(
    release: &GhRelease,
    version: &Version,
    platform: Platform,
) -> UpdateRelease {
    UpdateRelease {
        version: version.to_string(),
        tag: release.tag_name.clone(),
        prerelease: is_prerelease(release, version),
        published_at: release.published_at.clone(),
        notes: truncate_chars(release.body.as_deref().unwrap_or(""), NOTES_MAX_CHARS),
        html_url: release.html_url.clone(),
        asset: pick_asset(release, platform).map(|a| UpdateAsset {
            name: a.name.clone(),
            size: a.size,
        }),
    }
}

/// The answer to `check_update`. Prereleases count only when `include_prerelease` (the frontend
/// turns it on by default for a prerelease build, but an explicit 「不包含测试版」 is obeyed).
pub fn evaluate(
    releases: &[GhRelease],
    current: &Version,
    include_prerelease: bool,
    platform: Platform,
) -> UpdateCheck {
    let latest = select_latest(releases, include_prerelease);
    let update_available = latest
        .as_ref()
        .is_some_and(|(_, v)| v.cmp_precedence(current) == Ordering::Greater);
    UpdateCheck {
        current: current.to_string(),
        latest: latest.map(|(r, v)| to_update_release(r, &v, platform)),
        update_available,
    }
}

/// `sha256sum` / `shasum -a 256` lines (`<hex>  <name>`, `<hex> *<name>`): lower-case hash and
/// name. A UTF-8 BOM, CRLF and blank or malformed lines are tolerated.
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (hash, rest) = line.split_once(char::is_whitespace)?;
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let name = rest.trim_start();
            let name = name.strip_prefix('*').unwrap_or(name).trim_end();
            (!name.is_empty()).then(|| (hash.to_ascii_lowercase(), name.to_owned()))
        })
        .collect()
}

/// The hash `text` lists for `name`, if any.
pub fn expected_sha256(text: &str, name: &str) -> Option<String> {
    parse_sums(text)
        .into_iter()
        .find(|(_, n)| n == name)
        .map(|(hash, _)| hash)
}

/// Where a download named `name` lands in `dir`: `name`, else `stem (1).ext`, `stem (2).ext`, …;
/// `exists` says whether a path is taken.
pub fn free_target(dir: &Path, name: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !exists(&first) {
        return first;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_owned());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !exists(p))
        .unwrap_or(first)
}

// ---- network ----

pub fn user_agent(version: &Version) -> String {
    format!("Quicktext/{version}")
}

fn client(user_agent: &str, total: Option<Duration>) -> Result<reqwest::Client, CommandError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https" && attempt.previous().len() < 10 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    if let Some(total) = total {
        builder = builder.timeout(total);
    }
    builder.build().map_err(|e| request_error(e, "client"))
}

fn request_error(error: reqwest::Error, stage: &str) -> CommandError {
    let timeout = error.is_timeout();
    let error = error.without_url();
    let mut detail = format!("{stage}: {error}");
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    if timeout {
        fail(kind::TIMEOUT, "连接 GitHub 超时，请检查网络后重试", detail)
    } else {
        fail(
            kind::NETWORK,
            "无法连接 GitHub，请检查网络连接（公司网络可能需要代理）后重试",
            detail,
        )
    }
}

fn status_error(response: &reqwest::Response, what: &str) -> CommandError {
    let status = response.status();
    let detail = format!("{what}: HTTP {}", status.as_u16());
    let exhausted = response
        .headers()
        .get("x-ratelimit-remaining")
        .is_some_and(|v| v.as_bytes() == b"0");
    if status == StatusCode::TOO_MANY_REQUESTS || (status == StatusCode::FORBIDDEN && exhausted) {
        fail(
            kind::RATE_LIMITED,
            "GitHub 暂时限制了访问次数，请过一段时间（通常不超过一小时）再检查更新",
            detail,
        )
    } else if status == StatusCode::NOT_FOUND {
        fail(
            kind::RELEASE_NOT_FOUND,
            "GitHub 上没有找到这个版本，它可能已被撤下",
            detail,
        )
    } else {
        fail(
            kind::HTTP_STATUS,
            format!("GitHub 返回错误（HTTP {}），请稍后再试", status.as_u16()),
            detail,
        )
    }
}

/// The body, refused once it is over `max`.
async fn read_limited(
    response: &mut reqwest::Response,
    max: u64,
    what: &str,
) -> Result<Vec<u8>, CommandError> {
    let too_large = || {
        fail(
            kind::INVALID_RESPONSE,
            "GitHub 返回的内容过大，已停止读取",
            format!("{what}: body over {max} bytes"),
        )
    };
    if response.content_length().is_some_and(|len| len > max) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| request_error(e, what))? {
        if (body.len() + chunk.len()) as u64 > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn get_api<T: serde::de::DeserializeOwned>(
    user_agent: &str,
    url: Url,
    what: &str,
) -> Result<T, CommandError> {
    let mut response = client(user_agent, Some(API_TIMEOUT))?
        .get(url)
        .header(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        )
        .header(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        )
        .send()
        .await
        .map_err(|e| request_error(e, what))?;
    if !response.status().is_success() {
        return Err(status_error(&response, what));
    }
    let body = read_limited(&mut response, API_MAX_BYTES, what).await?;
    serde_json::from_slice(&body).map_err(|e| {
        fail(
            kind::INVALID_RESPONSE,
            "GitHub 返回的发布信息无法识别，请稍后再试",
            format!("{what}: {e}"),
        )
    })
}

fn api_url(segments: &[&str], query: Option<&str>) -> Url {
    #[allow(clippy::expect_used)] // A constant.
    let mut url = Url::parse(API).expect("API is a valid URL");
    if let Ok(mut path) = url.path_segments_mut() {
        path.extend(segments);
    }
    url.set_query(query);
    url
}

/// The newest 50 releases, as `check_update` reads them.
pub async fn list_releases(user_agent: &str) -> Result<Vec<GhRelease>, CommandError> {
    get_api(
        user_agent,
        api_url(&["releases"], Some("per_page=50")),
        "releases",
    )
    .await
}

/// The release of `tag`, looked up again (a URL from the frontend is never used).
pub async fn release_by_tag(user_agent: &str, tag: &str) -> Result<GhRelease, CommandError> {
    if parse_tag(tag).is_none() {
        return Err(fail(
            kind::INVALID_TAG,
            "版本号格式不正确",
            format!("tag {tag:?} is not SemVer"),
        ));
    }
    let release: GhRelease = get_api(
        user_agent,
        api_url(&["releases", "tags", tag], None),
        "release",
    )
    .await?;
    if release.draft || release.tag_name != tag {
        return Err(fail(
            kind::RELEASE_NOT_FOUND,
            "GitHub 上没有找到这个版本，它可能已被撤下",
            format!(
                "release of {tag:?} is a draft or has tag {:?}",
                release.tag_name
            ),
        ));
    }
    Ok(release)
}

/// The checksum file's text.
async fn fetch_sums(user_agent: &str, sums: &GhAsset) -> Result<String, CommandError> {
    let what = "checksums";
    let url = Url::parse(&sums.browser_download_url).map_err(|e| {
        fail(
            kind::INVALID_RESPONSE,
            "GitHub 返回的下载地址无效",
            format!("{what}: {e}"),
        )
    })?;
    let mut response = client(user_agent, Some(API_TIMEOUT))?
        .get(url)
        .header(ACCEPT, HeaderValue::from_static("application/octet-stream"))
        .send()
        .await
        .map_err(|e| request_error(e, what))?;
    if !response.status().is_success() {
        return Err(status_error(&response, what));
    }
    let body = read_limited(&mut response, SUMS_MAX_BYTES, what).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Deletes the partial file unless [`PartFile::keep`] was called.
struct PartFile {
    path: PathBuf,
    armed: bool,
}

impl PartFile {
    fn keep(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Gives the finished `part` its final name in `dir` (`name`, else `name (1)`, …) without ever
/// replacing anything there: a hard link fails when the name is taken (no check-then-rename race).
/// Where hard links are not supported, the content is copied into a file created exclusively.
/// `part` is removed on success.
fn publish_no_clobber(part: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    let taken = |p: &Path| fs::symlink_metadata(p).is_ok();
    for _ in 0..1000 {
        let target = free_target(dir, name, taken);
        match fs::hard_link(part, &target) {
            Ok(()) => {
                let _ = fs::remove_file(part);
                return Ok(target);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {}
        }
        let mut out = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(out) => out,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let copied = fs::File::open(part)
            .and_then(|mut src| io::copy(&mut src, &mut out))
            .and_then(|_| out.sync_all());
        drop(out);
        if let Err(e) = copied {
            let _ = fs::remove_file(&target);
            return Err(e);
        }
        let _ = fs::remove_file(part);
        return Ok(target);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free file name for the download",
    ))
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Streams `asset` into `dir` as `<name>.part`, checks it against `sums` and moves it to its final
/// name (an existing file with the same content is reused; a different one is never overwritten).
/// `progress` is called with (downloaded, total) now and then and once at the end.
pub async fn download(
    user_agent: &str,
    asset: &GhAsset,
    sums: Option<&GhAsset>,
    dir: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<UpdateDownload, CommandError> {
    let what = "download";
    if !is_downloadable(asset) {
        return Err(fail(
            kind::NO_ASSET,
            "这个版本的安装包信息无效，无法下载",
            format!("asset {:?} is not downloadable", asset.name),
        ));
    }
    if asset.size > ASSET_MAX_BYTES {
        return Err(fail(
            kind::TOO_LARGE,
            "安装包大小异常，已拒绝下载",
            format!("asset size {}", asset.size),
        ));
    }
    let url = Url::parse(&asset.browser_download_url).map_err(|e| {
        fail(
            kind::INVALID_RESPONSE,
            "GitHub 返回的下载地址无效",
            format!("{what}: {e}"),
        )
    })?;
    fs::create_dir_all(dir).map_err(|e| CommandError::io(IoOp::CreateDir, Some(dir), &e))?;
    let started = Instant::now();
    let mut response = client(user_agent, None)?
        .get(url)
        .header(ACCEPT, HeaderValue::from_static("application/octet-stream"))
        .send()
        .await
        .map_err(|e| request_error(e, what))?;
    if !response.status().is_success() {
        return Err(status_error(&response, what));
    }
    let total = response
        .content_length()
        .or((asset.size > 0).then_some(asset.size));
    let limit = total.unwrap_or(ASSET_MAX_BYTES).min(ASSET_MAX_BYTES);
    if total.is_some_and(|t| t > ASSET_MAX_BYTES) {
        return Err(fail(
            kind::TOO_LARGE,
            "安装包大小异常，已拒绝下载",
            format!("content length {total:?}"),
        ));
    }

    // A fresh name, created exclusively: an existing file (or a symlink planted under the name)
    // is never opened, truncated or followed, and only this file is ever removed again.
    let part_path = dir.join(format!(
        ".{}.{}.part",
        asset.name,
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part_path)
        .map_err(|e| CommandError::io(IoOp::Write, Some(&part_path), &e))?;
    let part = PartFile {
        path: part_path.clone(),
        armed: true,
    };
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    let mut last = Instant::now();
    progress(0, total);
    while let Some(chunk) = response.chunk().await.map_err(|e| request_error(e, what))? {
        downloaded += chunk.len() as u64;
        if downloaded > limit {
            return Err(fail(
                kind::TOO_LARGE,
                "下载的内容比安装包声明的大小更大，已停止下载",
                format!("over {limit} bytes"),
            ));
        }
        file.write_all(&chunk)
            .map_err(|e| CommandError::io(IoOp::Write, Some(&part_path), &e))?;
        hasher.update(&chunk);
        if last.elapsed() >= PROGRESS_INTERVAL {
            last = Instant::now();
            progress(downloaded, total);
        }
    }
    file.flush()
        .and_then(|()| file.sync_all())
        .map_err(|e| CommandError::io(IoOp::Write, Some(&part_path), &e))?;
    drop(file);
    progress(downloaded, total);
    if total.is_some_and(|t| t != downloaded) {
        return Err(fail(
            kind::INCOMPLETE,
            "下载没有完成（连接中断），请重试",
            format!("got {downloaded} of {total:?} bytes"),
        ));
    }
    let sha256 = hex(&hasher.finalize());

    let verified = match sums {
        Some(sums) => {
            let text = fetch_sums(user_agent, sums).await?;
            match expected_sha256(&text, &asset.name) {
                Some(expected) if expected == sha256 => true,
                Some(expected) => {
                    return Err(fail(
                        kind::CHECKSUM_MISMATCH,
                        "下载的文件校验失败（SHA-256 与发布页不一致），已删除，请重新下载",
                        format!("expected {expected}, got {sha256}"),
                    ))
                }
                None => {
                    tracing::warn!(event = "update.unverified", asset = %asset.name, "checksum file does not list the asset");
                    false
                }
            }
        }
        None => {
            tracing::warn!(event = "update.unverified", asset = %asset.name, "release has no checksum file");
            false
        }
    };

    // An existing file with this name and content is the same download: reuse it.
    // Only a regular file, not a symlink to one, counts as the same download.
    let same = dir.join(&asset.name);
    let target = if is_regular_file(&same) && sha256_file(&same).is_ok_and(|h| h == sha256) {
        drop(part); // Removes the partial copy.
        same
    } else {
        let target = publish_no_clobber(&part.path, dir, &asset.name)
            .map_err(|e| CommandError::io(IoOp::Rename, Some(&dir.join(&asset.name)), &e))?;
        part.keep(); // Already gone: published under `target`.
        target
    };
    tracing::info!(
        event = "update.downloaded",
        asset = %asset.name,
        bytes = downloaded,
        verified,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "update downloaded"
    );
    Ok(UpdateDownload {
        path: target.to_string_lossy().into_owned(),
        sha256,
        verified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> GhAsset {
        GhAsset {
            name: name.into(),
            size: 10,
            browser_download_url: format!("{DOWNLOAD_PREFIX}v0/{name}"),
        }
    }

    fn release(tag: &str, prerelease: bool) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            published_at: Some("2026-11-01T00:00:00Z".into()),
            body: Some(format!("notes of {tag}")),
            html_url: format!("https://github.com/meruchan0623/quick-text/releases/tag/{tag}"),
            assets: vec![
                asset(&format!(
                    "Quicktext_{}_aarch64.dmg",
                    &tag[1.min(tag.len())..]
                )),
                asset("SHA256SUMS-macos.txt"),
                asset(&format!(
                    "Quicktext-{}-Windows-x64-Portable.zip",
                    &tag[1.min(tag.len())..]
                )),
                asset("SHA256SUMS-windows.txt"),
            ],
        }
    }

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn sample() -> Vec<GhRelease> {
        let mut draft = release("v9.0.0", false);
        draft.draft = true;
        vec![
            release("test-4-eb4eebb", true),
            draft,
            release("v0.4.0-test.12", true),
            release("v0.3.2", false),
            release("v0.4.0-beta.2", true),
            release("v0.4.0-beta.11", true),
            release("v0.3.10", false),
            release("not-a-version", false),
            release("v0.3.1", false),
        ]
    }

    /// SemVer precedence, not string order or date; drafts and non-SemVer tags never count.
    #[test]
    fn the_latest_is_the_greatest_semver() {
        let releases = sample();
        let (r, ver) = select_latest(&releases, false).unwrap();
        assert_eq!((r.tag_name.as_str(), ver), ("v0.3.10", v("0.3.10")));
        // 0.4.0-test.12 > 0.4.0-beta.11 > 0.4.0-beta.2 (identifiers compare as strings, then
        // numbers numerically).
        let (r, _) = select_latest(&releases, true).unwrap();
        assert_eq!(r.tag_name, "v0.4.0-test.12");
        assert!(select_latest(&[release("test-3-ddff827", true)], true).is_none());
    }

    /// A plain tag flagged as a GitHub pre-release is a prerelease too.
    #[test]
    fn the_github_prerelease_flag_counts() {
        let releases = vec![release("v0.5.0", true), release("v0.4.0", false)];
        assert_eq!(
            select_latest(&releases, false).unwrap().0.tag_name,
            "v0.4.0"
        );
        assert_eq!(select_latest(&releases, true).unwrap().0.tag_name, "v0.5.0");
    }

    /// Build metadata does not change precedence: the first of equals wins.
    #[test]
    fn build_metadata_is_ignored() {
        let releases = vec![
            release("v1.0.0+sha.b", false),
            release("v1.0.0+sha.a", false),
        ];
        assert_eq!(
            select_latest(&releases, false).unwrap().0.tag_name,
            "v1.0.0+sha.b"
        );
        let check = evaluate(&releases, &v("1.0.0+other"), false, Platform::Other);
        assert!(!check.update_available);
    }

    #[test]
    fn evaluate_compares_with_the_current_version() {
        let releases = sample();
        let check = evaluate(&releases, &v("0.3.1"), false, Platform::MacosArm64);
        assert!(check.update_available);
        assert_eq!(check.current, "0.3.1");
        let latest = check.latest.unwrap();
        assert_eq!(latest.version, "0.3.10");
        assert!(!latest.prerelease);
        assert_eq!(latest.asset.unwrap().name, "Quicktext_0.3.10_aarch64.dmg");

        let check = evaluate(&releases, &v("0.3.10"), false, Platform::WindowsX64);
        assert!(!check.update_available);
        assert_eq!(check.latest.unwrap().version, "0.3.10");

        // A prerelease build that opted out of prereleases is offered only stable releases.
        let check = evaluate(&releases, &v("0.4.0-beta.2"), false, Platform::WindowsX64);
        assert!(!check.update_available);
        let latest = check.latest.unwrap();
        assert_eq!(latest.version, "0.3.10");
        assert!(!latest.prerelease);
        let check = evaluate(&releases, &v("0.3.1-test.1"), false, Platform::WindowsX64);
        assert!(check.update_available);
        assert_eq!(check.latest.unwrap().version, "0.3.10");

        // With prereleases asked for, the newest test build wins.
        let check = evaluate(&releases, &v("0.4.0-beta.2"), true, Platform::WindowsX64);
        assert!(check.update_available);
        let latest = check.latest.unwrap();
        assert_eq!(latest.version, "0.4.0-test.12");
        assert!(latest.prerelease);
        assert_eq!(
            latest.asset.unwrap().name,
            "Quicktext-0.4.0-test.12-Windows-x64-Portable.zip"
        );

        // Newer than everything published.
        let check = evaluate(&releases, &v("1.0.0"), true, Platform::Other);
        assert!(!check.update_available);
        assert_eq!(check.latest.unwrap().asset, None);
        assert_eq!(
            evaluate(&[], &v("0.3.1"), true, Platform::Other),
            UpdateCheck {
                current: "0.3.1".into(),
                latest: None,
                update_available: false
            }
        );
    }

    #[test]
    fn assets_are_picked_per_platform_and_only_from_this_repository() {
        let mut r = release("v1.0.0", false);
        r.assets.push(asset("Quicktext_1.0.0_x64.dmg"));
        assert_eq!(
            pick_asset(&r, Platform::MacosArm64).unwrap().name,
            "Quicktext_1.0.0_aarch64.dmg"
        );
        r.assets.retain(|a| !a.name.contains("aarch64"));
        assert!(
            pick_asset(&r, Platform::MacosArm64).is_none(),
            "an x64 dmg is not for arm64"
        );
        r.assets.push(asset("Quicktext.dmg"));
        assert_eq!(
            pick_asset(&r, Platform::MacosArm64).unwrap().name,
            "Quicktext.dmg"
        );
        assert_eq!(
            pick_asset(&r, Platform::WindowsX64).unwrap().name,
            "Quicktext-1.0.0-Windows-x64-Portable.zip"
        );
        assert!(pick_asset(&r, Platform::Other).is_none());
        assert_eq!(
            pick_sums(&r, Platform::WindowsX64).unwrap().name,
            "SHA256SUMS-windows.txt"
        );

        let mut foreign = release("v1.0.0", false);
        for a in &mut foreign.assets {
            a.browser_download_url = format!("https://example.com/{}", a.name);
        }
        assert!(pick_asset(&foreign, Platform::WindowsX64).is_none());
        assert!(pick_sums(&foreign, Platform::WindowsX64).is_none());

        let mut sneaky = release("v1.0.0", false);
        sneaky.assets = vec![asset("../Quicktext-1.0.0-Windows-x64-Portable.zip")];
        assert!(pick_asset(&sneaky, Platform::WindowsX64).is_none());
    }

    #[test]
    fn file_names_are_checked() {
        for ok in [
            "Quicktext_0.3.1_aarch64.dmg",
            "Quicktext-0.4.0-test.12-Windows-x64-Portable.zip",
        ] {
            assert!(is_safe_file_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".hidden",
            "a/b.zip",
            "a\\b.zip",
            "C:x.zip",
            "x.zip.",
            "x\u{0}.zip",
            "con?.zip",
        ] {
            assert!(!is_safe_file_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn tags_parse_with_or_without_v() {
        assert_eq!(parse_tag("v0.3.1"), Some(v("0.3.1")));
        assert_eq!(
            parse_tag("0.4.0-rc.1+sha.abc"),
            Some(v("0.4.0-rc.1+sha.abc"))
        );
        for bad in [
            "test-3-ddff827",
            "v1.2",
            "v01.2.3",
            "v1.2.3-",
            "vv1.2.3",
            "",
        ] {
            assert_eq!(parse_tag(bad), None, "{bad}");
        }
    }

    #[test]
    fn checksum_files_are_parsed() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let text = format!(
            "\u{feff}{a}  Quicktext_0.3.1_aarch64.dmg\r\n\r\n{b} *Quicktext-0.3.1-Windows-x64-Portable.zip\nnot a line\n{}  short.zip\n",
            "c".repeat(63)
        );
        assert_eq!(
            parse_sums(&text),
            vec![
                (a.clone(), "Quicktext_0.3.1_aarch64.dmg".into()),
                (
                    b.to_lowercase(),
                    "Quicktext-0.3.1-Windows-x64-Portable.zip".into()
                ),
            ]
        );
        assert_eq!(
            expected_sha256(&text, "Quicktext_0.3.1_aarch64.dmg"),
            Some(a)
        );
        assert_eq!(
            expected_sha256(&text, "Quicktext-0.3.1-Windows-x64-Portable.zip"),
            Some(b.to_lowercase())
        );
        assert_eq!(expected_sha256(&text, "short.zip"), None);
        assert_eq!(expected_sha256(&text, "missing.dmg"), None);
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qt-update-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn publishing_never_replaces_an_existing_file() {
        let dir = scratch("publish");
        fs::write(dir.join("pkg.zip"), b"mine").unwrap();
        let part = dir.join(".pkg.zip.x.part");
        fs::write(&part, b"new").unwrap();
        let target = publish_no_clobber(&part, &dir, "pkg.zip").unwrap();
        assert_eq!(target, dir.join("pkg (1).zip"));
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(dir.join("pkg.zip")).unwrap(), b"mine");
        assert!(!part.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_under_the_name_is_neither_followed_nor_reused() {
        let dir = scratch("symlink");
        let victim = dir.join("victim.txt");
        fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("pkg.zip")).unwrap();
        assert!(!is_regular_file(&dir.join("pkg.zip")));
        let part = dir.join(".pkg.zip.y.part");
        fs::write(&part, b"new").unwrap();
        let target = publish_no_clobber(&part, &dir, "pkg.zip").unwrap();
        assert_eq!(target, dir.join("pkg (1).zip"));
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_name_gets_a_suffix() {
        let dir = Path::new("/d");
        let taken = [PathBuf::from("/d/Q.dmg"), PathBuf::from("/d/Q (1).dmg")];
        assert_eq!(
            free_target(dir, "Q.dmg", |p| taken.iter().any(|t| t == p)),
            PathBuf::from("/d/Q (2).dmg")
        );
        assert_eq!(
            free_target(dir, "R.dmg", |p| taken.iter().any(|t| t == p)),
            PathBuf::from("/d/R.dmg")
        );
        assert_eq!(
            free_target(dir, "Q", |p| p == Path::new("/d/Q")),
            PathBuf::from("/d/Q (1)")
        );
    }

    #[test]
    fn long_notes_are_cut() {
        let mut r = release("v1.0.0", false);
        r.body = Some("字".repeat(NOTES_MAX_CHARS + 5));
        let out = to_update_release(&r, &v("1.0.0"), Platform::Other);
        assert_eq!(out.notes.chars().count(), NOTES_MAX_CHARS + 1);
        r.body = None;
        assert_eq!(
            to_update_release(&r, &v("1.0.0"), Platform::Other).notes,
            ""
        );
    }

    #[test]
    fn api_urls_escape_the_tag() {
        assert_eq!(
            api_url(&["releases"], Some("per_page=50")).as_str(),
            "https://api.github.com/repos/meruchan0623/quick-text/releases?per_page=50"
        );
        assert_eq!(
            api_url(&["releases", "tags", "v0.4.0-rc.1+b"], None).as_str(),
            "https://api.github.com/repos/meruchan0623/quick-text/releases/tags/v0.4.0-rc.1+b"
        );
    }

    #[test]
    fn errors_carry_the_kind_and_the_chinese_message() {
        let e = serde_json::to_value(fail(kind::NO_ASSET, "没有安装包", "detail")).unwrap();
        assert_eq!(
            e,
            serde_json::json!({"code":"update","detail":{"kind":"no_asset","message":"没有安装包"}})
        );
    }

    /// The whole path against the real GitHub (manual: needs the network). Downloads the Windows
    /// package of an existing release into a temporary folder and verifies it twice (the second
    /// download reuses the identical file).
    /// `cargo test -p quicktext live_github -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the network"]
    fn live_github_check_and_download() {
        let ua = user_agent(&v("0.0.0"));
        tauri::async_runtime::block_on(async {
            let releases = list_releases(&ua).await.unwrap();
            assert!(!releases.is_empty());
            let check = evaluate(&releases, &v("0.0.0"), true, Platform::WindowsX64);
            println!("{check:?}");
            let tag =
                std::env::var("QUICKTEXT_LIVE_TAG").unwrap_or_else(|_| "test-4-eb4eebb".into());
            let release: GhRelease =
                get_api(&ua, api_url(&["releases", "tags", &tag], None), "release")
                    .await
                    .unwrap();
            let asset = pick_asset(&release, Platform::WindowsX64).unwrap();
            let sums = pick_sums(&release, Platform::WindowsX64);
            let dir =
                std::env::temp_dir().join(format!("quicktext-update-{}", uuid::Uuid::new_v4()));
            let mut calls = 0;
            let first = download(&ua, asset, sums, &dir, |_, _| calls += 1)
                .await
                .unwrap();
            assert!(first.verified, "{first:?}");
            assert!(calls >= 2);
            let second = download(&ua, asset, sums, &dir, |_, _| {}).await.unwrap();
            assert_eq!(first, second);
            assert_eq!(
                std::fs::read_dir(&dir).unwrap().count(),
                1,
                "no .part left, no copy"
            );
            std::fs::remove_dir_all(dir).unwrap();
        });
    }
}

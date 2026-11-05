//! The download itself (docs/cloud-phrases.md §1, §3): one GET with the sync key,
//! conditional on the cached `ETag`, a 15 second budget and a 2 MiB body limit that is
//! enforced while the body streams in. No lock is held here.
//!
//! TLS goes through the platform (reqwest `native-tls`: SChannel on Windows,
//! Security.framework on macOS), so company proxies' root certificates in the system
//! store are trusted as by the browser. Redirects are not followed: a redirect could
//! carry the key to another address or downgrade to plain HTTP.
use super::{code, SyncError};
use reqwest::{
    header::{HeaderValue, ACCEPT, AUTHORIZATION, ETAG, IF_NONE_MATCH},
    StatusCode, Url,
};
use std::time::Duration;

/// Appended to the server address (`base_url` without trailing `/`, and without this
/// path when it was pasted in already).
pub const DOWNLOAD_PATH: &str = "/integrations/quick-text/v1/phrase-table";
/// Followed by the asset name; next to [`DOWNLOAD_PATH`] on the same server.
const ASSET_PATH: &str = "/integrations/quick-text/v1/phrase-assets/";
const USER_AGENT: &str = concat!("Quicktext/", env!("CARGO_PKG_VERSION"));

/// The budget of one download.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchLimits {
    /// For the whole request: connecting, waiting and reading the body.
    pub timeout: Duration,
    /// Largest body accepted; a longer one is `too_large` as soon as it is known.
    pub max_bytes: u64,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(15),
            max_bytes: 2 * 1024 * 1024,
        }
    }
}

/// What the server answered.
#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    /// 2xx: the body and its `ETag`.
    Modified { body: Vec<u8>, etag: Option<String> },
    /// 304: the cached table is current.
    NotModified,
}

/// The download URL for `base_url`: trailing `/` removed, [`DOWNLOAD_PATH`] appended. A
/// full download URL pasted as the address is taken as its server address.
/// Only `https`, or `http` to `localhost`, `127.0.0.1` or `[::1]` (local development);
/// no user name or password, query or fragment. An empty address is `not_configured`.
pub fn endpoint(base_url: &str) -> Result<Url, SyncError> {
    let base = base_url.trim().trim_end_matches('/');
    let base = base
        .strip_suffix(DOWNLOAD_PATH)
        .unwrap_or(base)
        .trim_end_matches('/');
    if base.is_empty() {
        return Err(SyncError::new(
            code::NOT_CONFIGURED,
            "未填写服务器地址",
            "no server address is set",
        ));
    }
    let invalid = |message: &str, detail: &str| SyncError::new(code::INVALID_URL, message, detail);
    let not_a_url = || invalid("服务器地址格式不正确", "not a valid URL");
    let parsed = Url::parse(base).map_err(|_| not_a_url())?;
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(not_a_url());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid(
            "服务器地址里不能包含用户名或密码",
            "user name or password in the URL",
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(invalid(
            "服务器地址里不能包含「?」或「#」后的部分",
            "query or fragment in the URL",
        ));
    }
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback(&parsed) => {}
        "http" => {
            return Err(invalid(
                "服务器地址必须以 https:// 开头（http:// 只能用于本机调试地址 localhost、127.0.0.1、[::1]）",
                "plain http to a host other than the loopback names",
            ))
        }
        _ => {
            return Err(invalid(
                "服务器地址必须以 https:// 开头",
                "scheme other than https",
            ))
        }
    }
    let url = Url::parse(&format!("{base}{DOWNLOAD_PATH}"))
        .ok()
        .filter(|url| url.path().ends_with(DOWNLOAD_PATH) && url.host() == parsed.host())
        .ok_or_else(not_a_url)?;
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

/// One download. `key` goes into `Authorization: Bearer` (marked sensitive, so no
/// `Debug` of the request shows it); `etag` into `If-None-Match`.
pub async fn fetch(
    url: &Url,
    key: &str,
    etag: Option<&str>,
    limits: &FetchLimits,
) -> Result<Fetched, SyncError> {
    let mut request = authorized_get(url, key, limits)?.header(ACCEPT, "application/json");
    if let Some(etag) = etag.and_then(|etag| HeaderValue::from_str(etag).ok()) {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| request_error(e, "request", limits))?;
    let status = response.status();
    if status == StatusCode::NOT_MODIFIED {
        return Ok(Fetched::NotModified);
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(SyncError::new(
            code::UNAUTHORIZED,
            "同步密钥无效或已作废，请向管理员索取新的同步密钥",
            format!("HTTP {}", status.as_u16()),
        ));
    }
    if !status.is_success() {
        let message = if status == StatusCode::NOT_FOUND {
            "服务器上没有团队话术接口（HTTP 404），请检查服务器地址是否填对".to_string()
        } else {
            format!(
                "服务器返回错误（HTTP {}），请稍后再试；一直失败时请联系管理员",
                status.as_u16()
            )
        };
        return Err(SyncError::new(
            code::HTTP_STATUS,
            message,
            format!("HTTP {}", status.as_u16()),
        ));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let too_large = || {
        SyncError::new(
            code::TOO_LARGE,
            format!(
                "团队话术表超过 {} 的大小上限，未下载",
                size_text(limits.max_bytes)
            ),
            format!("body larger than {} bytes", limits.max_bytes),
        )
    };
    let body = read_limited(&mut response, limits)
        .await?
        .ok_or_else(too_large)?;
    Ok(Fetched::Modified { body, etag })
}

/// A GET with the download rules (docs/cloud-phrases.md §3): the time budget, no redirects, the
/// User-Agent, the sync key marked sensitive; no proxy for a local development server.
fn authorized_get(
    url: &Url,
    key: &str,
    limits: &FetchLimits,
) -> Result<reqwest::RequestBuilder, SyncError> {
    let mut builder = reqwest::Client::builder()
        .timeout(limits.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT);
    if is_loopback(url) {
        // A local development server is never reached through a proxy.
        builder = builder.no_proxy();
    }
    let client = builder
        .build()
        .map_err(|e| request_error(e, "client", limits))?;
    let mut auth = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
        SyncError::new(
            code::NOT_CONFIGURED,
            "同步密钥含有无效字符，请重新填写",
            "the sync key contains characters a header cannot carry",
        )
    })?;
    auth.set_sensitive(true);
    Ok(client.get(url.clone()).header(AUTHORIZATION, auth))
}

/// The body, `None` as soon as it is known to be over `limits.max_bytes`: announced, or while it
/// streams in (dropping the response closes the connection, nothing more is read).
async fn read_limited(
    response: &mut reqwest::Response,
    limits: &FetchLimits,
) -> Result<Option<Vec<u8>>, SyncError> {
    if response
        .content_length()
        .is_some_and(|len| len > limits.max_bytes)
    {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| request_error(e, "body", limits))?
    {
        if (body.len() + chunk.len()) as u64 > limits.max_bytes {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// Where `asset` is for the table at `table_url`: the same server, the sibling path. `asset` has
/// passed `is_asset_name`, so it needs no escaping.
pub fn asset_url(table_url: &Url, asset: &str) -> Result<Url, SyncError> {
    let path = table_url.path();
    let base = path.strip_suffix(DOWNLOAD_PATH).unwrap_or(path);
    let mut url = table_url.clone();
    url.set_path(&format!("{base}{ASSET_PATH}{asset}"));
    Ok(url)
}

/// One image (docs/image-and-zip-package.md §5), under the rules of the table download; the body
/// limit is the asset limit. Failures are coded like the table's: `unauthorized` for 401 and 403,
/// else `network`, `timeout`, `http_status` or `too_large`. Messages are the caller's to replace.
pub async fn fetch_asset(
    table_url: &Url,
    key: &str,
    asset: &str,
    limits: &FetchLimits,
) -> Result<Vec<u8>, SyncError> {
    let url = asset_url(table_url, asset)?;
    let limits = FetchLimits {
        timeout: limits.timeout,
        max_bytes: quicker_domain::MAX_ASSET_BYTES,
    };
    let mut response = authorized_get(&url, key, &limits)?
        .send()
        .await
        .map_err(|e| request_error(e, "request", &limits))?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(SyncError::new(
            code::UNAUTHORIZED,
            "同步密钥无效或已作废，请向管理员索取新的同步密钥",
            format!("HTTP {}", status.as_u16()),
        ));
    }
    if !status.is_success() {
        return Err(SyncError::new(
            code::HTTP_STATUS,
            "服务器没有返回图片",
            format!("HTTP {}", status.as_u16()),
        ));
    }
    read_limited(&mut response, &limits).await?.ok_or_else(|| {
        SyncError::new(
            code::TOO_LARGE,
            "图片超过 10 MiB",
            format!("body larger than {} bytes", limits.max_bytes),
        )
    })
}

/// `timeout` or `network`. The detail is reqwest's error chain without the URL (the
/// address is in the settings already).
fn request_error(error: reqwest::Error, stage: &str, limits: &FetchLimits) -> SyncError {
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
        SyncError::new(
            code::TIMEOUT,
            format!(
                "连接服务器超时（{} 秒内没有完成），请稍后重试",
                limits.timeout.as_secs_f32()
            ),
            detail,
        )
    } else {
        SyncError::new(
            code::NETWORK,
            "无法连接服务器，请检查网络和服务器地址",
            detail,
        )
    }
}

/// `2 MiB`, `64 KiB`, `512 字节`.
fn size_text(bytes: u64) -> String {
    if bytes >= 1024 * 1024 && bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} 字节")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::tests::{http, FakeServer, Reply};

    fn limits() -> FetchLimits {
        FetchLimits {
            timeout: Duration::from_millis(500),
            max_bytes: 1024,
        }
    }
    fn run(server: &FakeServer, etag: Option<&str>) -> Result<Fetched, SyncError> {
        let url = endpoint(&server.base_url()).unwrap();
        tauri::async_runtime::block_on(fetch(&url, "qtk_abc.secret", etag, &limits()))
    }

    /// §3: only https, or http to the loopback names; the path is appended once.
    #[test]
    fn endpoint_accepts_https_and_loopback_http_only() {
        for (base, url) in [
            (
                "https://csapi.tsim.tech",
                "https://csapi.tsim.tech/integrations/quick-text/v1/phrase-table",
            ),
            (
                " https://csapi.tsim.tech/api/// ",
                "https://csapi.tsim.tech/api/integrations/quick-text/v1/phrase-table",
            ),
            (
                "http://localhost:8000",
                "http://localhost:8000/integrations/quick-text/v1/phrase-table",
            ),
            (
                "http://127.0.0.1:9/",
                "http://127.0.0.1:9/integrations/quick-text/v1/phrase-table",
            ),
            (
                "http://[::1]:8000",
                "http://[::1]:8000/integrations/quick-text/v1/phrase-table",
            ),
            (
                "HTTPS://CSAPI.TSIM.TECH",
                "https://csapi.tsim.tech/integrations/quick-text/v1/phrase-table",
            ),
            (
                "http://localhost:8000/integrations/quick-text/v1/phrase-table",
                "http://localhost:8000/integrations/quick-text/v1/phrase-table",
            ),
            (
                "https://csapi.tsim.tech/api/integrations/quick-text/v1/phrase-table/",
                "https://csapi.tsim.tech/api/integrations/quick-text/v1/phrase-table",
            ),
        ] {
            assert_eq!(endpoint(base).unwrap().as_str(), url, "{base}");
        }
        assert_eq!(endpoint("  ").unwrap_err().code, code::NOT_CONFIGURED);
        for base in [
            "http://csapi.tsim.tech",
            "http://127.0.0.2",
            "http://localhost.evil.example",
            "ftp://csapi.tsim.tech",
            "csapi.tsim.tech",
            "https://user:pass@csapi.tsim.tech",
            "https://csapi.tsim.tech/?a=1",
            "https://csapi.tsim.tech#x",
            "https://",
            "https:///integrations/quick-text/v1/phrase-table",
            "not a url",
        ] {
            assert_eq!(
                endpoint(base).unwrap_err().code,
                code::INVALID_URL,
                "{base}"
            );
        }
    }

    /// §1, §3: 200 gives the body and the ETag; the request carries the key, the cached
    /// ETag and the User-Agent.
    #[test]
    fn a_200_returns_the_body_and_the_etag() {
        let server = FakeServer::start(vec![Reply::now(http(
            "200 OK",
            &[("ETag", "\"abc\""), ("Content-Type", "application/json")],
            b"{\"x\":1}",
        ))]);
        assert_eq!(
            run(&server, Some("\"old\"")).unwrap(),
            Fetched::Modified {
                body: b"{\"x\":1}".to_vec(),
                etag: Some("\"abc\"".into())
            }
        );
        let request = server.requests().remove(0).to_ascii_lowercase();
        assert!(request.starts_with("get /integrations/quick-text/v1/phrase-table http/1.1"));
        assert!(request.contains("authorization: bearer qtk_abc.secret"));
        assert!(request.contains("if-none-match: \"old\""));
        assert!(request.contains(&format!(
            "user-agent: quicktext/{}",
            env!("CARGO_PKG_VERSION")
        )));
    }

    /// 304 is "not modified"; 401 and 403 are `unauthorized`; other statuses and
    /// redirects `http_status`.
    #[test]
    fn statuses_map_to_their_codes() {
        let server = FakeServer::start(vec![
            Reply::now(http("304 Not Modified", &[("ETag", "\"abc\"")], b"")),
            Reply::now(http(
                "401 Unauthorized",
                &[("WWW-Authenticate", "Bearer")],
                b"{\"detail\":{\"code\":\"unauthorized\"}}",
            )),
            Reply::now(http("403 Forbidden", &[], b"")),
            Reply::now(http("404 Not Found", &[], b"")),
            Reply::now(http("500 Internal Server Error", &[], b"oops")),
            Reply::now(http(
                "302 Found",
                &[("Location", "http://example.invalid/")],
                b"",
            )),
        ]);
        assert_eq!(run(&server, None).unwrap(), Fetched::NotModified);
        assert_eq!(run(&server, None).unwrap_err().code, code::UNAUTHORIZED);
        assert_eq!(run(&server, None).unwrap_err().code, code::UNAUTHORIZED);
        let error = run(&server, None).unwrap_err();
        assert_eq!(
            (error.code, error.detail.as_str()),
            (code::HTTP_STATUS, "HTTP 404")
        );
        assert!(error.message.contains("检查服务器地址"), "{error}");
        let error = run(&server, None).unwrap_err();
        assert_eq!(
            (error.code, error.detail.as_str()),
            (code::HTTP_STATUS, "HTTP 500")
        );
        assert_eq!(
            error.message,
            "服务器返回错误（HTTP 500），请稍后再试；一直失败时请联系管理员"
        );
        assert_eq!(run(&server, None).unwrap_err().code, code::HTTP_STATUS);
        assert_eq!(server.requests().len(), 6, "the redirect was not followed");
    }

    /// §3: a body over the limit is `too_large`, whether announced or streamed.
    #[test]
    fn bodies_over_the_limit_are_too_large() {
        let big = vec![b'x'; 4096];
        let mut streamed = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        streamed.extend_from_slice(&big);
        let server = FakeServer::start(vec![
            Reply::now(http("200 OK", &[], &big)),
            Reply::now(streamed),
            Reply::now(http("200 OK", &[], &big[..1024])),
        ]);
        let error = run(&server, None).unwrap_err();
        assert_eq!(error.code, code::TOO_LARGE);
        assert_eq!(error.message, "团队话术表超过 1 KiB 的大小上限，未下载");
        assert_eq!(run(&server, None).unwrap_err().code, code::TOO_LARGE);
        assert_eq!(size_text(2 * 1024 * 1024), "2 MiB");
        assert!(
            matches!(run(&server, None), Ok(Fetched::Modified { body, .. }) if body.len() == 1024)
        );
    }

    /// §3: no answer within the budget is `timeout`; nobody listening is `network`.
    #[test]
    fn silence_is_a_timeout_and_a_refused_connection_is_network() {
        let server = FakeServer::start(vec![Reply::after(
            Duration::from_secs(3),
            http("200 OK", &[], b"{}"),
        )]);
        let started = std::time::Instant::now();
        let error = run(&server, None).unwrap_err();
        assert_eq!(error.code, code::TIMEOUT, "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let url = endpoint(&format!("http://127.0.0.1:{port}")).unwrap();
        let error = tauri::async_runtime::block_on(fetch(&url, "k", None, &limits())).unwrap_err();
        assert_eq!(error.code, code::NETWORK, "{error}");
        assert_eq!(error.message, "无法连接服务器，请检查网络和服务器地址");
        assert!(!error.detail.contains("127.0.0.1"), "{error}");
    }

    /// TLS through the platform stack (manual: needs the network).
    /// `cargo test -p quicktext tls_handshake -- --ignored`
    #[test]
    #[ignore = "needs the network"]
    fn tls_handshake_with_the_platform_stack() {
        let url = Url::parse("https://example.com/").unwrap();
        let fetched =
            tauri::async_runtime::block_on(fetch(&url, "k", None, &FetchLimits::default()));
        assert!(
            matches!(fetched, Ok(Fetched::Modified { .. }))
                || matches!(&fetched, Err(e) if e.code == code::HTTP_STATUS),
            "{fetched:?}"
        );
    }
}

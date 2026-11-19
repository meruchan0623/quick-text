//! Update feed and the shapes of the settings page's manual check (docs/auto-update.md).
//!
//! Updates come from the two TSim servers, production and test
//! (`/mnt/project-files/client-updates/CONTRACT.md` §6): each serves a tauri updater v2 manifest
//! at `/client-updates/v1/quicktext/<channel>/<macos|windows>/latest.json` (204 when nothing is
//! published). Both addresses are compiled in; a manifest cannot point the client elsewhere for
//! the next check. Every check reads both, and the newest version wins ([`pick_newest`]; the same
//! version on both: production). One server failing does not fail the check.
//!
//! The runtime (download, minisign verification, install) is [`crate::autoupdate`]; the background
//! loop and the manual check in 设置 share it.
use crate::error::CommandError;
use semver::Version;
use serde::Serialize;

/// Product segment of the update paths.
pub const PRODUCT: &str = "quicktext";
/// The update servers, production first: it wins a tie.
pub const SERVERS: [(Source, &str); 2] = [
    (Source::Production, "https://csapi.tsim.tech"),
    (Source::Test, "https://test-csapi.tsim.tech"),
];
/// Frontend event with an [`UpdateProgress`] payload.
pub const PROGRESS_EVENT: &str = "update-download-progress";

/// Which server announced an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Production,
    Test,
}

/// The platform segment of the update paths for this build; `None` where nothing is published.
pub fn platform_segment() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("macos")
    } else if cfg!(windows) {
        Some("windows")
    } else {
        None
    }
}

/// The manifest addresses of a channel, production first; `None` for an unknown channel.
pub fn manifest_urls(channel: &str, platform: &str) -> Option<[(Source, String); 2]> {
    if channel != "stable" || !matches!(platform, "macos" | "windows") {
        return None;
    }
    Some(SERVERS.map(|(source, base)| {
        (
            source,
            format!("{base}/client-updates/v1/{PRODUCT}/{channel}/{platform}/latest.json"),
        )
    }))
}

/// The newest of the servers' answers. `Ok(None)`: no server has a newer version (at least one
/// answered); `Err`: every server failed. On equal versions the earlier answer (production) wins.
pub fn pick_newest<T, E>(
    answers: impl IntoIterator<Item = Result<Option<(Version, T)>, E>>,
) -> Result<Option<(Version, T)>, Vec<E>> {
    let mut errors = Vec::new();
    let mut answered = false;
    let mut best: Option<(Version, T)> = None;
    for answer in answers {
        match answer {
            Err(e) => errors.push(e),
            Ok(found) => {
                answered = true;
                if let Some((version, item)) = found {
                    if best.as_ref().is_none_or(|(b, _)| version > *b) {
                        best = Some((version, item));
                    }
                }
            }
        }
    }
    if answered {
        Ok(best)
    } else {
        Err(errors)
    }
}

/// `CommandError::Update.kind` values.
pub mod kind {
    /// This build has no update channel (local builds) or no platform package.
    pub const DISABLED: &str = "disabled";
    /// Neither server could be read.
    pub const NETWORK: &str = "network";
    /// Download or signature verification failed.
    pub const DOWNLOAD: &str = "download";
    /// A check or download is already running.
    pub const BUSY: &str = "busy";
    /// The version asked for is not the one the last check found.
    pub const STALE: &str = "stale";
    /// Nothing downloaded to install.
    pub const NOT_READY: &str = "not_ready";
    pub const INSTALL: &str = "install";
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

// ---- the IPC shapes (src/lib/update.ts) ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    /// Plain text, as published.
    pub notes: String,
    /// RFC 3339.
    pub pub_date: Option<String>,
    pub source: Source,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateCheck {
    pub current: String,
    /// The newer version found; `None` when this build is the newest.
    pub latest: Option<UpdateInfo>,
    pub update_available: bool,
    /// `latest` is downloaded and verified: 「重启并更新」 installs it.
    pub ready: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateProgress {
    pub version: String,
    pub downloaded: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateDownload {
    /// Downloaded and verified against the embedded public key.
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn both_servers_are_fixed_and_production_comes_first() {
        let [(first, prod), (second, test)] = manifest_urls("stable", "macos").unwrap();
        assert_eq!(first, Source::Production);
        assert_eq!(
            prod,
            "https://csapi.tsim.tech/client-updates/v1/quicktext/stable/macos/latest.json"
        );
        assert_eq!(second, Source::Test);
        assert_eq!(
            test,
            "https://test-csapi.tsim.tech/client-updates/v1/quicktext/stable/macos/latest.json"
        );
        let [(_, win), _] = manifest_urls("stable", "windows").unwrap();
        assert!(win.ends_with("/quicktext/stable/windows/latest.json"));
        assert!(manifest_urls("beta", "macos").is_none());
        assert!(manifest_urls("stable", "linux").is_none());
    }

    #[test]
    fn no_github_address_is_left() {
        for (_, base) in SERVERS {
            assert!(base.starts_with("https://") && !base.contains("github"));
        }
    }

    #[test]
    fn the_newest_wins_and_production_wins_a_tie() {
        let picked = pick_newest::<_, ()>([
            Ok(Some((v("0.5.1"), "prod"))),
            Ok(Some((v("0.5.2"), "test"))),
        ]);
        assert_eq!(picked.unwrap().unwrap().1, "test");
        let tie = pick_newest::<_, ()>([
            Ok(Some((v("0.5.2"), "prod"))),
            Ok(Some((v("0.5.2"), "test"))),
        ]);
        assert_eq!(tie.unwrap().unwrap().1, "prod");
        let pre = pick_newest::<_, ()>([
            Ok(Some((v("0.6.0-rc.1"), "prod"))),
            Ok(Some((v("0.6.0"), "test"))),
        ]);
        assert_eq!(pre.unwrap().unwrap().1, "test");
    }

    #[test]
    fn one_server_failing_does_not_fail_the_check() {
        let picked = pick_newest([Err("down"), Ok(Some((v("0.5.1"), "test")))]);
        assert_eq!(picked.unwrap().unwrap().1, "test");
        let none = pick_newest::<&str, _>([Ok(None), Err("down")]);
        assert_eq!(none.unwrap(), None);
        let up_to_date = pick_newest::<&str, ()>([Ok(None), Ok(None)]);
        assert_eq!(up_to_date.unwrap(), None);
        let all_failed = pick_newest::<&str, _>([Err("a"), Err("b")]);
        assert_eq!(all_failed.unwrap_err(), vec!["a", "b"]);
    }

    #[test]
    fn the_ipc_shapes_are_snake_case() {
        let check = UpdateCheck {
            current: "0.5.0".into(),
            latest: Some(UpdateInfo {
                version: "0.5.1".into(),
                notes: "说明".into(),
                pub_date: None,
                source: Source::Test,
            }),
            update_available: true,
            ready: false,
        };
        let json = serde_json::to_value(&check).unwrap();
        assert_eq!(json["latest"]["source"], "test");
        assert_eq!(json["update_available"], true);
        assert_eq!(json["ready"], false);
    }
}

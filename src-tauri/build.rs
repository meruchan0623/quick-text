//! Build script of the app: Tauri's own build step, plus a record of which
//! frontend this build embeds (FE-18 / B-52; Codex review 1 of the 0.4
//! release hygiene).
//!
//! `tauri::generate_context!` embeds the directory named by
//! `build.frontendDist` (`../dist`) into the exe. The packaging scripts take an
//! exe that was built earlier, so the `dist/` they find in the tree may not be
//! the frontend inside it (rebuilt since, or never the one compiled in). This
//! script hashes the directory Tauri embeds and hands the digest to the crate
//! as `QUICKER_TEXT_FRONTEND_DIST_SHA256`; `src/main.rs` keeps it in the
//! binary as `quicker-text-frontend-dist-sha256:<64 hex>`, and
//! `scripts/check-embedded-frontend.ps1` compares it with the directory it is
//! about to scan for the mock backend.
//!
//! Digest (the script computes the same thing): for every file under the
//! directory, the line `<SHA-256 of the file, lowercase hex>  <path relative
//! to the directory, '/'-separated>\n`; the lines sorted by path (ordinal over
//! UTF-16 code units, which is .NET's `StringComparer.Ordinal`); the digest is
//! the SHA-256 of the concatenated lines, lowercase hex. It covers the files as
//! they are on disk: Tauri rewrites HTML as it embeds it (CSP hashes), so the
//! embedded bytes differ from them.
//!
//! When the directory cannot be hashed (no `frontendDist` directory in the
//! configuration, directory missing, a link inside it (Vite writes none, Tauri
//! would follow it), an unreadable file or a non-Unicode name) the value is
//! `unavailable` and cargo prints a warning. The build itself goes on: debug
//! builds and tests do not need the record, and the packaging check fails on
//! it.
//!
//! In Tauri's development mode the value is `dev-mode` and no directory is
//! hashed (Codex review 2 of the 0.4 release hygiene): such an exe embeds no
//! frontend, so the digest of `dist/` would vouch for something it does not
//! contain. The packaging check refuses it. See [`DEV_MODE`] for how the mode
//! is told.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};
use tauri_utils::config::FrontendDist;

const DIGEST_ENV: &str = "QUICKER_TEXT_FRONTEND_DIST_SHA256";

/// The record of an exe built in Tauri's development mode, which embeds no
/// frontend.
///
/// The mode is told with `tauri_build::is_dev()`, which reads `cargo:dev` from
/// the build script of the `tauri` crate: `true` when `tauri` is compiled
/// without its `custom-protocol` feature (this crate's default feature
/// `custom-protocol` turns it on; `--no-default-features` turns it off).
/// `generate_context!` decides with the same feature
/// (tauri-macros 2.7: `dev: cfg!(not(feature = "custom-protocol"))`, and
/// `tauri/custom-protocol` enables `tauri-macros/custom-protocol`), so
/// `is_dev()` false means the macro embeds `frontendDist`. In development mode
/// tauri-codegen embeds an empty asset set when `build.devUrl` is set (it is:
/// the exe loads http://localhost:1420); without a devUrl it would embed the
/// directory, but it is still a development build (dev CSP), not one to ship,
/// so both cases get this value.
const DEV_MODE: &str = "dev-mode";

fn main() {
    let record = if tauri_build::is_dev() {
        // A legitimate build against the dev server; no warning.
        DEV_MODE.to_owned()
    } else {
        let digest = frontend_dist().and_then(|dist| {
            // A directory is scanned recursively for changes; a missing path makes
            // the script run on every build, so the record never goes stale.
            println!("cargo:rerun-if-changed={}", dist.display());
            dist_digest(&dist)
        });
        digest.unwrap_or_else(|reason| {
            println!("cargo:warning=frontend digest unavailable: {reason}");
            "unavailable".to_owned()
        })
    };
    println!("cargo:rustc-env={DIGEST_ENV}={record}");
    tauri_build::build()
}

/// The directory Tauri embeds: `build.frontendDist` read the way tauri-build
/// and tauri-codegen read it (tauri.conf.json, the platform file, then the
/// `TAURI_CONFIG` merge patch the Tauri CLI passes), relative to this crate.
fn frontend_dist() -> Result<PathBuf, String> {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let target = env::var("TARGET").map_err(|e| e.to_string())?;
    let target = tauri_utils::platform::Target::from_triple(&target);
    let (config, _) = tauri_utils::config::parse::read_from(target, &root)
        .map_err(|e| format!("Tauri configuration: {e}"))?;
    let mut value = config.pointer("/build/frontendDist").cloned();
    if let Ok(inline) = env::var("TAURI_CONFIG") {
        let inline: serde_json::Value =
            serde_json::from_str(&inline).map_err(|e| format!("TAURI_CONFIG: {e}"))?;
        if let Some(patched) = inline.pointer("/build/frontendDist") {
            value = Some(patched.clone());
        }
    }
    let dist = match value {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<FrontendDist>(value)
                .map_err(|e| format!("build.frontendDist: {e}"))?,
        ),
    };
    match dist {
        Some(FrontendDist::Directory(path)) => Ok(root.join(path)),
        Some(other) => Err(format!("build.frontendDist is not a directory: {other}")),
        None => Err("build.frontendDist is not set".to_owned()),
    }
}

fn dist_digest(dist: &Path) -> Result<String, String> {
    if !dist.is_dir() {
        return Err(format!("{} is not a directory", dist.display()));
    }
    let mut files = Vec::new();
    collect_files(dist, "", &mut files)?;
    files.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
    let mut listing = String::new();
    for (relative, path) in &files {
        let bytes = fs::read(path).map_err(|e| format!("{relative}: {e}"))?;
        let _ = writeln!(listing, "{}  {relative}", hex(&Sha256::digest(&bytes)));
    }
    Ok(hex(&Sha256::digest(listing.as_bytes())))
}

fn collect_files(
    dir: &Path,
    prefix: &str,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|name| format!("file name is not Unicode: {name:?}"))?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type().map_err(|e| format!("{relative}: {e}"))?;
        if kind.is_symlink() {
            return Err(format!("{relative} is a link"));
        } else if kind.is_dir() {
            collect_files(&entry.path(), &relative, files)?;
        } else if kind.is_file() {
            files.push((relative, entry.path()));
        } else {
            return Err(format!("{relative} is not a regular file"));
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

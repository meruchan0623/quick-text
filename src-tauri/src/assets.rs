//! The asset library (docs/image-and-zip-package.md §2): `assets/<asset>` in the data directory,
//! content addressed, shared by personal rules and team phrases. Old revisions can still refer to
//! an image, so nothing here ever deletes a file.
//!
//! What a name promises is decided by `quicker_domain` ([`verify_asset`]); this module only stores
//! and finds the files.
use crate::{
    error::{CommandError, IoOp},
    store::replace_file,
};
use quicker_domain::*;
use serde::Serialize;
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

/// `import_image_asset`'s answer: the `asset` and `name` of an image action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportedAsset {
    pub asset: String,
    pub name: String,
}

/// The library directory of `data_dir`.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("assets")
}

/// Whether the library holds `asset`. Anything that is not an asset name is not in it and never
/// reaches the file system.
pub fn contains(data_dir: &Path, asset: &str) -> bool {
    is_asset_name(asset) && dir(data_dir).join(asset).is_file()
}

/// Puts verified `bytes` in the library under `asset`: temp file, sync, atomic rename. A file
/// already there with these bytes stays untouched; any other content (damage) is replaced. The
/// caller has checked `verify_asset`.
pub fn write(data_dir: &Path, asset: &str, bytes: &[u8]) -> io::Result<()> {
    if !is_asset_name(asset) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let library = dir(data_dir);
    let target = library.join(asset);
    if fs::metadata(&target).is_ok_and(|meta| meta.len() == bytes.len() as u64)
        && fs::read(&target).is_ok_and(|existing| existing == bytes)
    {
        return Ok(());
    }
    fs::create_dir_all(&library)?;
    let temp = library.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// `import_image_asset`: reads at most the size limit from `path`, whatever the file claims,
/// checks size and signature and copies it into the library. The display name is the file name,
/// cut to the limit of an image action.
pub fn import_file(data_dir: &Path, path: &Path) -> Result<ImportedAsset, CommandError> {
    let read_error = |e: io::Error| CommandError::io(IoOp::Read, Some(path), &e);
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(read_error)?
        .take(MAX_ASSET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    let asset = asset_name_for(&bytes)
        .map_err(|e| CommandError::invalid_file(e.code(), e.message(), path))?;
    write(data_dir, &asset, &bytes)
        .map_err(|e| CommandError::io(IoOp::Write, Some(Path::new(&asset)), &e))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .take(MAX_ASSET_DISPLAY_NAME_CHARS)
        .collect();
    Ok(ImportedAsset { asset, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage";

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quicker-assets-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn importing_copies_the_image_under_its_hash_whatever_the_file_is_called() {
        let data = temp_dir();
        let source = data.join("二维码.jpeg");
        fs::write(&source, PNG).unwrap();
        let imported = import_file(&data, &source).unwrap();
        assert_eq!(imported.name, "二维码.jpeg");
        assert_eq!(imported.asset, asset_name_for(PNG).unwrap());
        assert!(imported.asset.ends_with(".png"), "the signature decides");
        assert!(contains(&data, &imported.asset));
        // The rule no longer depends on the original file.
        fs::remove_file(&source).unwrap();
        assert_eq!(
            fs::read(dir(&data).join(&imported.asset)).unwrap(),
            PNG,
            "the library holds the bytes"
        );
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn the_display_name_is_cut_to_the_action_limit() {
        let data = temp_dir();
        let source = data.join(format!("{}.png", "图".repeat(80)));
        fs::write(&source, PNG).unwrap();
        assert_eq!(
            import_file(&data, &source).unwrap().name.chars().count(),
            84
        );
        // Longer than the file system's names allow is only possible through the cut itself.
        let long: String = "x".repeat(MAX_ASSET_DISPLAY_NAME_CHARS + 20);
        let source = data.join(&long);
        fs::write(&source, PNG).unwrap();
        let name = import_file(&data, &source).unwrap().name;
        assert_eq!(name.chars().count(), MAX_ASSET_DISPLAY_NAME_CHARS);
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn importing_refuses_what_is_not_an_acceptable_image() {
        let data = temp_dir();
        let text = data.join("note.png");
        fs::write(&text, b"just text").unwrap();
        let oversized = data.join("big.png");
        let mut big = PNG.to_vec();
        big.resize(MAX_ASSET_BYTES as usize + 1, 0);
        fs::write(&oversized, &big).unwrap();
        for (path, code) in [(text, "image_format"), (oversized, "image_too_large")] {
            let Err(CommandError::Validation { diagnostics }) = import_file(&data, &path) else {
                panic!("{code}: expected a validation error");
            };
            assert_eq!(diagnostics[0].code, code);
        }
        assert!(!dir(&data).exists(), "nothing was stored");
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn writing_keeps_good_files_and_replaces_damaged_ones() {
        let data = temp_dir();
        let asset = asset_name_for(PNG).unwrap();
        write(&data, &asset, PNG).unwrap();
        let path = dir(&data).join(&asset);
        let leftovers = || {
            fs::read_dir(dir(&data))
                .unwrap()
                .filter(|entry| {
                    let entry = entry.as_ref().unwrap();
                    entry.file_name().to_string_lossy().ends_with(".tmp")
                })
                .count()
        };
        write(&data, &asset, PNG).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PNG);
        fs::write(&path, b"\x89PNG\r\n\x1a\ndamaged!").unwrap();
        write(&data, &asset, PNG).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PNG);
        assert_eq!(leftovers(), 0);
        // A name that could leave the library is refused before the file system sees it.
        assert!(write(&data, "../escape.png", PNG).is_err());
        assert!(!contains(&data, "../escape.png"));
        assert!(!data.join("escape.png").exists());
        fs::remove_dir_all(data).unwrap();
    }
}

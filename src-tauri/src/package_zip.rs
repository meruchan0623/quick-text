//! The ZIP configuration package (docs/image-and-zip-package.md §3): `package.json` plus
//! `assets/<asset>` for each image it refers to.
//!
//! Reading treats the file as hostile. Only those two kinds of entry are ever read, and nothing is
//! extracted: names are compared, never joined to a path, so a traversal name is an entry that is
//! ignored. Limits count the bytes actually decompressed, because the sizes in ZIP headers are
//! whatever the author wrote. Any violation refuses the whole package before anything is stored;
//! writing the images is the importer's last step.
use crate::{assets, error::CommandError};
use quicker_domain::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::Path,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

const MANIFEST: &str = "package.json";
const ASSET_PREFIX: &str = "assets/";
/// The size of the file itself, far above what the limits below can legitimately produce; the
/// central directory is parsed before any of them apply.
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    manifest: u64,
    image: u64,
    total: u64,
}

const LIMITS: Limits = Limits {
    entries: 2100,
    manifest: 16 * 1024 * 1024,
    image: MAX_ASSET_BYTES,
    total: 128 * 1024 * 1024,
};

fn refuse(code: &str, message: impl Into<String>) -> Vec<Diagnostic> {
    vec![Diagnostic::error(code, message, SourceSpan::default())]
}

fn not_a_zip() -> Vec<Diagnostic> {
    refuse("package_zip", "文件不是有效的 ZIP 配置包，未导入任何内容")
}

/// What a ZIP file is, by its entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZipKind {
    Package,
    Workbook,
}

/// `import_config`'s detection: `package.json` makes a package, `xl/workbook.xml` a workbook.
pub fn classify(file: &mut fs::File) -> Result<ZipKind, Vec<Diagnostic>> {
    check_file_size(file)?;
    let zip = ZipArchive::new(&mut *file).map_err(|_| not_a_zip())?;
    let mut workbook = false;
    for name in zip.file_names() {
        let name = name.map_err(|_| not_a_zip())?;
        if name == MANIFEST {
            return Ok(ZipKind::Package);
        }
        workbook |= name == "xl/workbook.xml";
    }
    if workbook {
        Ok(ZipKind::Workbook)
    } else {
        Err(refuse(
            "package_manifest",
            "ZIP 中没有 package.json，不是配置包，未导入任何内容",
        ))
    }
}

fn check_file_size(file: &mut impl Seek) -> Result<(), Vec<Diagnostic>> {
    let size = file.seek(SeekFrom::End(0)).map_err(|_| not_a_zip())?;
    file.seek(SeekFrom::Start(0)).map_err(|_| not_a_zip())?;
    if size > MAX_FILE_BYTES {
        return Err(refuse("package_size", "配置包文件过大，未导入任何内容"));
    }
    Ok(())
}

/// A package that passed every rule of §3.
#[derive(Debug)]
pub struct Package {
    /// `package.json`; its content is `config_package`'s to judge.
    pub json: String,
    /// Each image, verified against its name.
    pub assets: BTreeMap<String, Vec<u8>>,
}

pub fn read(reader: impl Read + Seek) -> Result<Package, Vec<Diagnostic>> {
    read_with(reader, &LIMITS)
}

fn read_with(mut reader: impl Read + Seek, limits: &Limits) -> Result<Package, Vec<Diagnostic>> {
    check_file_size(&mut reader)?;
    let mut zip = ZipArchive::new(reader).map_err(|_| not_a_zip())?;
    if zip.len() > limits.entries {
        return Err(refuse(
            "package_entries",
            format!("配置包超过 {} 个条目，未导入任何内容", limits.entries),
        ));
    }
    // Headers only, before anything is decompressed. Every entry counts for encryption: a package
    // with parts that cannot be read is not the package its author meant.
    let mut wanted = vec![];
    for index in 0..zip.len() {
        let entry = zip.by_index_raw(index).map_err(|_| not_a_zip())?;
        if entry.encrypted() {
            return Err(refuse(
                "package_encrypted",
                "配置包含有加密条目，未导入任何内容",
            ));
        }
        let name = entry.name().map_err(|_| not_a_zip())?;
        let asset = name.strip_prefix(ASSET_PREFIX).filter(|a| is_asset_name(a));
        if name == MANIFEST || asset.is_some() {
            wanted.push((index, name.into_owned()));
        }
    }
    let mut json = None;
    let mut images = BTreeMap::new();
    let mut total = 0u64;
    for (index, name) in wanted {
        let limit = if name == MANIFEST {
            limits.manifest
        } else {
            limits.image
        };
        let entry = zip.by_index(index).map_err(|_| not_a_zip())?;
        let mut bytes = Vec::new();
        // One byte over the smaller limit tells which one was passed; the header is not asked.
        let allowance = limit.min(limits.total - total) + 1;
        entry
            .take(allowance)
            .read_to_end(&mut bytes)
            .map_err(|_| not_a_zip())?;
        let size = bytes.len() as u64;
        if size > limit {
            return Err(if name == MANIFEST {
                refuse("package_size", "package.json 超过 16 MiB，未导入任何内容")
            } else {
                refuse(
                    "package_image_size",
                    "配置包中有超过 10 MiB 的图片，未导入任何内容",
                )
            });
        }
        total += size;
        if total > limits.total {
            return Err(refuse(
                "package_total_size",
                "配置包解压后超过 128 MiB，未导入任何内容",
            ));
        }
        if name == MANIFEST {
            json = Some(String::from_utf8(bytes).map_err(|_| {
                refuse(
                    "package_json",
                    "package.json 不是 UTF-8 文本，未导入任何内容",
                )
            })?);
        } else {
            let asset = name.trim_start_matches(ASSET_PREFIX).to_string();
            verify_asset(&asset, &bytes).map_err(|e| {
                refuse(
                    e.code(),
                    format!("配置包中的图片有误：{}，未导入任何内容", e.message()),
                )
            })?;
            images.insert(asset, bytes);
        }
    }
    Ok(Package {
        json: json.ok_or_else(|| {
            refuse(
                "package_manifest",
                "ZIP 中没有 package.json，不是配置包，未导入任何内容",
            )
        })?,
        assets: images,
    })
}

/// The images the readable rules refer to. Unreadable entries are kept verbatim but never run, so
/// they do not bring files along.
fn referenced_assets(config: &Config) -> BTreeSet<&str> {
    config
        .rules
        .iter()
        .filter_map(|rule| match &rule.action {
            Action::Image { asset, .. } if is_asset_name(asset) => Some(asset.as_str()),
            _ => None,
        })
        .collect()
}

/// The ZIP of `package` (the package JSON of `config`) and the library files its rules refer to.
/// A file that is gone or damaged is left out and logged: the package would be refused at import
/// with a damaged one, and without it the rule imports disabled with a warning. Refuses a package
/// that could not be imported again, as the JSON export does.
pub fn build(data_dir: &Path, config: &Config, package: &str) -> Result<Vec<u8>, CommandError> {
    let too_big = |message: &str| {
        CommandError::validation(vec![Diagnostic::error(
            "package_size",
            message,
            SourceSpan::default(),
        )])
    };
    let library = assets::dir(data_dir);
    let mut images = vec![];
    let mut total = package.len() as u64;
    for asset in referenced_assets(config) {
        let path = library.join(asset);
        let usable = fs::metadata(&path).is_ok_and(|meta| meta.len() <= MAX_ASSET_BYTES);
        let bytes = usable.then(|| fs::read(&path).ok()).flatten();
        let Some(bytes) = bytes.filter(|bytes| verify_asset(asset, bytes).is_ok()) else {
            tracing::warn!(
                event = "export.asset_unavailable",
                asset,
                "image missing or damaged; exported without it"
            );
            continue;
        };
        total += bytes.len() as u64;
        if total > LIMITS.total {
            return Err(too_big(
                "配置包的图片超过 128 MiB，无法导出为可重新导入的配置",
            ));
        }
        images.push((asset, bytes));
    }
    if images.len() + 1 > LIMITS.entries {
        return Err(too_big("配置包的图片过多，无法导出为可重新导入的配置"));
    }
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    // Images are compressed already.
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file(MANIFEST, deflated)
        .map_err(CommandError::internal)?;
    zip.write_all(package.as_bytes())
        .map_err(CommandError::internal)?;
    for (asset, bytes) in images {
        zip.start_file(format!("{ASSET_PREFIX}{asset}"), stored)
            .map_err(CommandError::internal)?;
        zip.write_all(&bytes).map_err(CommandError::internal)?;
    }
    Ok(zip.finish().map_err(CommandError::internal)?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage";
    const SMALL: Limits = Limits {
        entries: 4,
        manifest: 128,
        image: 64,
        total: 200,
    };

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quicker-package-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png(fill: u8, size: usize) -> Vec<u8> {
        let mut bytes = PNG.to_vec();
        bytes.resize(size.max(PNG.len()), fill);
        bytes
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn entry_name(asset: &str) -> String {
        format!("{ASSET_PREFIX}{asset}")
    }

    fn code(result: Result<Package, Vec<Diagnostic>>) -> String {
        result.unwrap_err()[0].code.clone()
    }

    fn image_rule(id: &str, asset: &str) -> Rule {
        Rule {
            id: id.into(),
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: format!("t{id}"),
            },
            action: Action::Image {
                asset: asset.into(),
                name: "二维码.png".into(),
            },
            ..Rule::default()
        }
    }

    /// §3: export and read back, and what is left out of the package.
    #[test]
    fn a_package_round_trips_with_only_the_images_its_rules_use() {
        let data = temp_dir();
        let used = asset_name_for(PNG).unwrap();
        let unrelated = asset_name_for(&png(1, 32)).unwrap();
        let in_unreadable = asset_name_for(&png(2, 32)).unwrap();
        for (asset, bytes) in [
            (&used, PNG.to_vec()),
            (&unrelated, png(1, 32)),
            (&in_unreadable, png(2, 32)),
        ] {
            assets::write(&data, asset, &bytes).unwrap();
        }
        let config = Config {
            rules: vec![image_rule("a", &used)],
            unreadable_rules: vec![UnreadableRule {
                index: 1,
                raw: serde_json::json!({
                    "id": "future",
                    "action": { "type": "image", "asset": in_unreadable, "extra": 1 }
                }),
            }],
            ..Config::default()
        };
        let json = export_package(&config).unwrap();
        let bytes = build(&data, &config, &json).unwrap();
        let package = read(Cursor::new(&bytes)).unwrap();
        assert_eq!(package.json, json);
        assert_eq!(
            package.assets.keys().collect::<Vec<_>>(),
            [&used],
            "rules in unreadable_rules bring no files"
        );
        assert_eq!(package.assets[&used], PNG);
        let imported = crate::config_package::import(&package.json).unwrap();
        assert_eq!(imported.config.rules, config.rules);
        fs::remove_dir_all(data).unwrap();
    }

    /// A missing or damaged library file does not stop the export, and is not packed: a damaged one
    /// would make the whole package refused at import.
    #[test]
    fn an_unusable_library_file_is_left_out_of_the_export() {
        let data = temp_dir();
        let damaged = asset_name_for(PNG).unwrap();
        assets::write(&data, &damaged, b"\x89PNG\r\n\x1a\nother").unwrap();
        let gone = asset_name_for(&png(3, 32)).unwrap();
        let config = Config {
            rules: vec![image_rule("a", &damaged), image_rule("b", &gone)],
            ..Config::default()
        };
        let json = export_package(&config).unwrap();
        let package = read(Cursor::new(build(&data, &config, &json).unwrap())).unwrap();
        assert!(package.assets.is_empty());
        fs::remove_dir_all(data).unwrap();
    }

    /// §3: entries that are not `package.json` or `assets/<asset>` are ignored; they are never
    /// read, so a traversal name cannot be written anywhere and a damaged one cannot matter.
    #[test]
    fn other_entries_are_ignored_and_traversal_names_go_nowhere() {
        let asset = asset_name_for(PNG).unwrap();
        let bytes = zip_of(&[
            ("__MACOSX/._package.json", b"junk"),
            ("../../evil.png", PNG),
            ("/etc/evil.png", PNG),
            (&format!("assets/../{asset}"), PNG),
            (&format!("assets/sub/{asset}"), PNG),
            ("assets/not-a-hash.png", PNG),
            (&format!("ASSETS/{asset}"), PNG),
            (MANIFEST, b"{}"),
            (&entry_name(&asset), PNG),
        ]);
        let package = read(Cursor::new(bytes)).unwrap();
        assert_eq!(package.assets.keys().collect::<Vec<_>>(), [&asset]);
    }

    #[test]
    fn a_zip_without_a_manifest_or_not_a_zip_is_refused() {
        assert_eq!(
            code(read(Cursor::new(zip_of(&[("readme.txt", b"x")])))),
            "package_manifest"
        );
        assert_eq!(
            code(read(Cursor::new(b"PK not a zip".to_vec()))),
            "package_zip"
        );
        // Not UTF-8.
        assert_eq!(
            code(read(Cursor::new(zip_of(&[(MANIFEST, b"\xff\xfe")])))),
            "package_json"
        );
    }

    /// §3: the limits on entries and sizes, by what is read.
    #[test]
    fn each_limit_refuses_the_package() {
        let asset = |bytes: &[u8]| entry_name(&asset_name_for(bytes).unwrap());
        let many: Vec<(String, Vec<u8>)> = (0..5).map(|i| (format!("other{i}"), vec![])).collect();
        let entries: Vec<(&str, &[u8])> = many
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
            .collect();
        assert_eq!(
            code(read_with(Cursor::new(zip_of(&entries)), &SMALL)),
            "package_entries"
        );
        let large_json = vec![b' '; 129];
        assert_eq!(
            code(read_with(
                Cursor::new(zip_of(&[(MANIFEST, &large_json)])),
                &SMALL
            )),
            "package_size"
        );
        let image = png(0, 65);
        assert_eq!(
            code(read_with(
                Cursor::new(zip_of(&[(MANIFEST, b"{}"), (&asset(&image), &image)])),
                &SMALL
            )),
            "package_image_size"
        );
        // Each image passes on its own; together they do not.
        let (a, b, c, d) = (png(1, 64), png(2, 64), png(3, 64), png(4, 64));
        let limits = Limits {
            entries: 9,
            ..SMALL
        };
        assert_eq!(
            code(read_with(
                Cursor::new(zip_of(&[
                    (MANIFEST, b"{}"),
                    (&asset(&a), &a),
                    (&asset(&b), &b),
                    (&asset(&c), &c),
                    (&asset(&d), &d),
                ])),
                &limits
            )),
            "package_total_size"
        );
        // Exactly at the limits is fine.
        let package = read_with(
            Cursor::new(zip_of(&[(MANIFEST, b"{}"), (&asset(&a), &a)])),
            &SMALL,
        )
        .unwrap();
        assert_eq!(package.assets.len(), 1);
    }

    /// Overwrites the "uncompressed size" of every header: what a zip bomb claims.
    fn claim_small_sizes(bytes: &mut [u8]) {
        for at in 0..bytes.len().saturating_sub(4) {
            let offset = match &bytes[at..at + 4] {
                b"PK\x03\x04" => 22,
                b"PK\x01\x02" => 24,
                _ => continue,
            };
            bytes[at + offset..at + offset + 4].copy_from_slice(&16u32.to_le_bytes());
        }
    }

    /// §3: the limit is on the bytes read, not the size the headers declare.
    #[test]
    fn a_false_size_in_the_headers_does_not_get_past_the_limit() {
        let image = png(0, 4096);
        let name = entry_name(&asset_name_for(&image).unwrap());
        let mut bytes = zip_of(&[(MANIFEST, b"{}"), (&name, &image)]);
        claim_small_sizes(&mut bytes);
        assert!(read_with(
            Cursor::new(bytes),
            &Limits {
                image: 1024,
                total: 1 << 20,
                ..SMALL
            }
        )
        .is_err());
        // The real size over the real limit, deflated to almost nothing.
        let image = png(0, MAX_ASSET_BYTES as usize + 1);
        let name = entry_name(&asset_name_for(&image[..1024]).unwrap());
        let mut bytes = zip_of(&[(MANIFEST, b"{}"), (&name, &image)]);
        assert!(bytes.len() < 1024 * 1024);
        assert_eq!(code(read(Cursor::new(&bytes))), "package_image_size");
        claim_small_sizes(&mut bytes);
        assert!(read(Cursor::new(&bytes)).is_err());
    }

    /// §3: the hash and the extension in the name must fit the bytes.
    #[test]
    fn an_image_that_does_not_fit_its_name_refuses_the_package() {
        let asset = asset_name_for(PNG).unwrap();
        let other_hash = entry_name(&format!("{}.png", "0".repeat(64)));
        assert_eq!(
            code(read(Cursor::new(zip_of(&[
                (MANIFEST, b"{}"),
                (&other_hash, PNG)
            ])))),
            "image_hash"
        );
        let wrong_extension = entry_name(&asset.replace(".png", ".jpg"));
        assert_eq!(
            code(read(Cursor::new(zip_of(&[
                (MANIFEST, b"{}"),
                (&wrong_extension, PNG)
            ])))),
            "image_extension"
        );
        let text = entry_name(&format!("{}.png", "1".repeat(64)));
        assert_eq!(
            code(read(Cursor::new(zip_of(&[
                (MANIFEST, b"{}"),
                (&text, b"not an image")
            ])))),
            "image_format"
        );
    }

    /// §3: an encrypted entry refuses the package, also among the ignored ones.
    #[test]
    fn encrypted_entries_refuse_the_package() {
        for encrypted in [MANIFEST, "__MACOSX/x"] {
            let mut bytes = zip_of(&[(MANIFEST, b"{}"), ("__MACOSX/x", b"y")]);
            // General purpose flag, bit 0, in the central directory and local headers of the entry.
            let needle = encrypted.as_bytes();
            for at in 0..bytes.len().saturating_sub(46) {
                let (offset, name_at) = match &bytes[at..at + 4] {
                    b"PK\x03\x04" => (6, 30),
                    b"PK\x01\x02" => (8, 46),
                    _ => continue,
                };
                if bytes[at + name_at..].starts_with(needle) {
                    bytes[at + offset] |= 1;
                }
            }
            assert_eq!(
                code(read(Cursor::new(bytes))),
                "package_encrypted",
                "{encrypted}"
            );
        }
    }

    #[test]
    fn zip_files_are_told_apart_by_their_entries() {
        let dir = temp_dir();
        let open = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            fs::write(&path, bytes).unwrap();
            fs::File::open(path).unwrap()
        };
        let package = zip_of(&[(MANIFEST, b"{}")]);
        // The content decides, not the extension.
        assert_eq!(
            classify(&mut open("named.xlsx", &package)).unwrap(),
            ZipKind::Package
        );
        let workbook = zip_of(&[("xl/workbook.xml", b"<workbook/>")]);
        assert_eq!(
            classify(&mut open("named.zip", &workbook)).unwrap(),
            ZipKind::Workbook
        );
        let other = zip_of(&[("readme.txt", b"x")]);
        assert_eq!(
            classify(&mut open("other.zip", &other)).unwrap_err()[0].code,
            "package_manifest"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}

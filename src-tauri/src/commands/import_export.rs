//! File pickers. 0.5 adds `export` and 0.5.x `import_preview` / `import_commit`
//! here; the legacy `export_config` / `import_config` live in `legacy.rs`.
use crate::error::CommandError;

/// A file dialog filter: display name and extensions without the dot.
pub type Filter = (&'static str, &'static [&'static str]);

/// Configuration packages (`export_config`, `import_config`, `choose_file("import")`).
pub const CONFIG_FILTERS: &[Filter] = &[("JSON 配置", &["json"])];
/// Formats `preview_image` can decode: the `image` crate features in Cargo.toml.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "tif", "tiff", "webp",
];
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "csv", "tsv", "log", "html", "htm", "xml", "json",
];

/// Extension filters per `choose_file` kind (B-42). Text sources may use any
/// extension, so that kind keeps an "all files" choice; `files` (attachments
/// pasted as files) and unknown kinds are not filtered.
fn filters(kind: &str) -> &'static [Filter] {
    match kind {
        "import" => CONFIG_FILTERS,
        "image" => &[("图片", IMAGE_EXTENSIONS)],
        "text" => &[("文本文件", TEXT_EXTENSIONS), ("所有文件", &["*"])],
        _ => &[],
    }
}

pub fn dialog(filters: &[Filter]) -> rfd::FileDialog {
    filters
        .iter()
        .fold(rfd::FileDialog::new(), |dialog, &(name, extensions)| {
            dialog.add_filter(name, extensions)
        })
}

/// Returns `Ok(None)` when the user cancels the dialog.
#[tauri::command]
pub fn choose_file(kind: String) -> Result<Option<Vec<String>>, CommandError> {
    let dialog = dialog(filters(&kind));
    Ok(if kind == "files" {
        dialog.pick_files().map(|p| {
            p.into_iter()
                .map(|x| x.to_string_lossy().into_owned())
                .collect()
        })
    } else {
        dialog
            .pick_file()
            .map(|p| vec![p.to_string_lossy().into_owned()])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_follow_the_file_kind() {
        assert_eq!(filters("import"), CONFIG_FILTERS);
        assert_eq!(CONFIG_FILTERS[0].1, ["json"]);
        assert_eq!(filters("image")[0].1, IMAGE_EXTENSIONS);
        assert!(filters("text").iter().any(|(_, x)| x == &["*"]));
        assert!(filters("files").is_empty());
        assert!(filters("unknown").is_empty());
    }

    #[test]
    fn image_filter_lists_only_decodable_formats() {
        for extension in IMAGE_EXTENSIONS {
            let format = image::ImageFormat::from_extension(extension)
                .unwrap_or_else(|| panic!("{extension} is not an image extension"));
            assert!(format.reading_enabled(), "{extension} decoder is disabled");
        }
    }
}

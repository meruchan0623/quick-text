//! File pickers. 0.5 adds `export` and 0.5.x `import_preview` / `import_commit`
//! here; the legacy `export_config` / `import_config` live in `legacy.rs`.

#[tauri::command]
pub fn choose_file(kind: String) -> Option<Vec<String>> {
    let dialog = rfd::FileDialog::new();
    if kind == "files" {
        dialog.pick_files().map(|p| {
            p.into_iter()
                .map(|x| x.to_string_lossy().into_owned())
                .collect()
        })
    } else {
        dialog
            .pick_file()
            .map(|p| vec![p.to_string_lossy().into_owned()])
    }
}

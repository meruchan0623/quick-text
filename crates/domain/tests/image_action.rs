//! The 「粘贴图片」 action (docs/image-and-zip-package.md §1).
use quicker_domain::*;
use serde_json::json;
use std::path::PathBuf;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage";

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "quicker-image-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn context(dir: &std::path::Path) -> VariableContext {
    VariableContext {
        assets_dir: Some(dir.to_path_buf()),
        ..VariableContext::default()
    }
}

fn image(asset: &str) -> Action {
    Action::Image {
        asset: asset.into(),
        name: "二维码.png".into(),
    }
}

fn first_code(result: Result<ActionPlan, Vec<Diagnostic>>) -> String {
    result.unwrap_err()[0].code.clone()
}

#[test]
fn the_action_round_trips_in_the_contract_shape() {
    let asset = asset_name_for(PNG).unwrap();
    let wire = json!({ "type": "image", "asset": asset, "name": "二维码.png" });
    let action: Action = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(action, image(&asset));
    assert_eq!(serde_json::to_value(&action).unwrap(), wire);
    // `name` is optional and defaults to the empty string.
    let action: Action =
        serde_json::from_value(json!({ "type": "image", "asset": asset })).unwrap();
    assert_eq!(
        action,
        Action::Image {
            asset: asset.clone(),
            name: String::new()
        }
    );
    // Strict like every action: an unknown field is not this action.
    assert!(serde_json::from_value::<Action>(json!({
        "type": "image", "asset": asset, "future": 1
    }))
    .is_err());
}

/// A rule being edited has `asset: ""`: it must read as a rule (not become unreadable) and be
/// reported by validation, at the field.
#[test]
fn a_wrong_asset_reads_and_is_reported_by_validation() {
    let raw = json!({
        "version": 1,
        "rules": [{
            "id": "editing",
            "match": { "kind": "literal", "value": "qr" },
            "action": { "type": "image", "asset": "" }
        }]
    });
    let config = read_stored_config(raw.to_string().as_bytes())
        .unwrap()
        .config;
    assert!(config.unreadable_rules.is_empty());
    assert_eq!(config.rules.len(), 1);
    let errors = compile_config(&config).unwrap_err();
    assert_eq!(errors[0].code, IMAGE_ASSET);
    assert!(errors[0].path.ends_with("asset"), "{}", errors[0].path);
    for bad in ["", "x.png", "../../etc/passwd"] {
        assert_eq!(validate_action(&image(bad))[0].code, IMAGE_ASSET, "{bad}");
    }
}

#[test]
fn the_display_name_is_limited_on_save() {
    let asset = asset_name_for(PNG).unwrap();
    let named = |chars: usize| Action::Image {
        asset: asset.clone(),
        name: "图".repeat(chars),
    };
    assert!(validate_action(&named(MAX_ASSET_DISPLAY_NAME_CHARS)).is_empty());
    let errors = validate_action(&named(MAX_ASSET_DISPLAY_NAME_CHARS + 1));
    assert_eq!(errors[0].code, IMAGE_NAME);
}

/// Old clients: 0.3.1's reader has no such action. The tolerance is the reader's, so an action
/// type from a release newer than a client lands in the unreadable rules or `unreadable_count`,
/// and its neighbours survive; `schema_version` stays 1 (compat_0_3_1.rs has the 0.3.1 model).
#[test]
fn an_action_type_newer_than_the_reader_is_counted_not_fatal() {
    let future = json!({
        "id": "future", "match": { "kind": "literal", "value": "ff" },
        "action": { "type": "video", "asset": "x" }
    });
    let image = json!({
        "id": "image", "match": { "kind": "literal", "value": "ii" },
        "action": { "type": "image", "asset": asset_name_for(PNG).unwrap() }
    });
    let table = parse_cloud_table(
        json!({
            "format": CLOUD_FORMAT, "schema_version": 1, "revision": 1, "updated_at": null,
            "rules": [future, image]
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(table.schema_version, CLOUD_SCHEMA_VERSION);
    assert_eq!(table.unreadable_count, 1);
    assert_eq!(table.rules[0].id, "image");
}

#[test]
fn the_plan_pastes_the_library_file() {
    let dir = temp_dir();
    let asset = asset_name_for(PNG).unwrap();
    std::fs::write(dir.join(&asset), PNG).unwrap();
    let plan = preflight_action(&image(&asset), &context(&dir)).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(
        plan.steps[0].operation,
        Operation::PasteImage {
            path: dir.join(&asset).to_string_lossy().into_owned()
        }
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// §1: these all fail in preflight, which runs before the abbreviation is erased.
#[test]
fn missing_or_damaged_files_fail_before_erasure() {
    let dir = temp_dir();
    let asset = asset_name_for(PNG).unwrap();
    let missing = preflight_action(&image(&asset), &context(&dir)).unwrap_err();
    assert_eq!(missing[0].code, "image_missing");
    // The text reaches the run's result as it is, so it is the user's language.
    assert_eq!(missing[0].message, "图片文件不在资源库中，请重新选择图片");
    // Without a library the run cannot find any file.
    assert_eq!(
        first_code(preflight_action(
            &image(&asset),
            &VariableContext::default()
        )),
        "image_library"
    );
    // Wrong signature.
    std::fs::write(dir.join(&asset), b"not an image").unwrap();
    assert_eq!(
        first_code(preflight_action(&image(&asset), &context(&dir))),
        "image_format"
    );
    // The signature is fine, the bytes are not the ones the name promises.
    std::fs::write(dir.join(&asset), b"\x89PNG\r\n\x1a\ntruncat").unwrap();
    assert_eq!(
        first_code(preflight_action(&image(&asset), &context(&dir))),
        "image_corrupt"
    );
    // A name that could leave the library never reaches the file system.
    assert_eq!(
        first_code(preflight_action(&image("../secret.png"), &context(&dir))),
        IMAGE_ASSET
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A display name is never a template: however it reads, a run does not touch the clipboard or
/// the selection for it (a table could otherwise trigger a selection capture).
#[test]
fn the_display_name_never_reads_local_data() {
    let action = Action::Image {
        asset: asset_name_for(PNG).unwrap(),
        name: "{selection} {cliptext}".into(),
    };
    let config = Config {
        rules: vec![Rule {
            id: "image".into(),
            action: action.clone(),
            ..Rule::default()
        }],
        ..Config::default()
    };
    assert!(!action_uses_variable(&action, &config, "selection"));
    assert!(!action_uses_variable(&action, &config, "cliptext"));
    let reusable = Action::Reusable {
        action_id: "image".into(),
        parameters: Default::default(),
    };
    assert!(!action_uses_variable(&reusable, &config, "selection"));
}

/// B-09: the hash is not user content, the original file name is.
#[test]
fn debug_output_redacts_the_display_name() {
    let shown = format!("{:?}", image("abc"));
    assert!(shown.contains("abc"), "{shown}");
    assert!(!shown.contains("二维码"), "{shown}");
}

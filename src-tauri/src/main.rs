#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app_state;
mod autostart;
mod commands;
mod diagnostics;
mod error;
mod events;
mod store;
mod tray;

use app_state::Managed;
use quicker_platform::ExecutionResult;
use std::{path::PathBuf, sync::Arc};
use tauri::Manager;

fn main() {
    diagnostics::panic::install();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // Release isolation is available only in the explicit input acceptance mode.
            let acceptance_dir =
                if cfg!(debug_assertions) || quicker_platform::acceptance_test_input_enabled() {
                    std::env::var_os("QUICKER_TEXT_DATA_DIR")
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                } else {
                    None
                };
            let dir = match acceptance_dir {
                Some(dir) => dir,
                None => app.path().app_data_dir()?,
            };
            // Logging comes first so the store and every command can log
            // (`CommandError::internal` trace ids must be findable).
            diagnostics::logging::init(&dir);
            diagnostics::panic::set_crash_dir(dir.clone());
            tracing::info!(
                event = "app.start",
                version = env!("CARGO_PKG_VERSION"),
                debug_build = cfg!(debug_assertions),
                "Quicker Text starting"
            );
            app.manage(diagnostics::level::LogLevelControl::start(
                dir.clone(),
                Arc::new(quicker_platform::traits::SystemClock),
                Arc::new(diagnostics::logging::apply_level),
            ));
            let store = store::Store::load(dir).map_err(std::io::Error::other)?;
            let engine = store.engine.clone();
            app.manage(Managed::new(store));
            tray::spawn_status_watcher(app.handle().clone(), engine.clone());
            if let Err(message) = quicker_platform::start_engine(engine.clone()) {
                *engine.last.lock().unwrap() = Some(ExecutionResult {
                    id: "engine".into(),
                    status: "blocked".into(),
                    message,
                    completed_steps: 0,
                    side_effects: false,
                });
            }
            tray::build(app)?;
            if std::env::args().any(|x| x == "--background") {
                if let Some(w) = app.get_webview_window("main") {
                    w.hide()?;
                }
            }
            Ok(())
        })
        .on_window_event(|w, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = w.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::runtime::load_state,
            commands::legacy::save_draft,
            commands::legacy::apply_settings,
            commands::legacy::discard_draft,
            commands::legacy::rollback,
            commands::runtime::set_paused,
            commands::rules::validate_config,
            commands::rules::test_rule,
            commands::rules::parse_source,
            commands::runtime::cancel_execution,
            commands::runtime::execute_action,
            commands::legacy::get_foreground,
            commands::import_export::choose_file,
            commands::legacy::export_config,
            commands::legacy::import_config,
            commands::runtime::start_recording,
            commands::runtime::stop_recording,
            commands::settings::set_autostart,
            commands::rules::preview_file,
            commands::rules::preview_image,
            commands::system::get_cursor,
            commands::system::capture_selection,
            commands::system::exit_app,
            commands::diagnostics::set_log_level
        ])
        .build(tauri::generate_context!())
        .expect("Quicker Text startup failed")
        .run(|_, event| {
            if let tauri::RunEvent::Exit = event {
                tracing::info!(event = "app.exit", "Quicker Text exiting");
                diagnostics::logging::flush();
            }
        });
}

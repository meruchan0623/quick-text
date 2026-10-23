#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app_state;
mod autostart;
mod commands;
mod diagnostics;
mod error;
mod events;
mod faults;
mod startup;
mod store;
mod tray;

use app_state::{DataDir, Managed};
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
            app.manage(DataDir(dir.clone()));
            app.manage(commands::diagnostics::BundledAssets::embedded(
                &app.asset_resolver(),
            ));
            app.manage(diagnostics::level::LogLevelControl::start(
                dir.clone(),
                Arc::new(quicker_platform::traits::SystemClock),
                Arc::new(diagnostics::logging::apply_level),
            ));
            store::prefs::apply_clipboard_prefs(&dir);
            let store = store::Store::load(dir);
            let engine = store.engine.clone();
            let runtime = store.runtime.clone();
            app.manage(Managed::new(store));
            // The tray and the EventBus come before anything that can
            // change the engine or find a problem, so the frontend and the
            // tray miss nothing after the state `load_state` reads.
            let pause_item = tray::build(app)?;
            let status = tray::TrayStatus::new(app.handle().clone(), pause_item, engine.clone());
            let bus = {
                let managed = app.state::<Managed>();
                let mut store = managed.lock();
                let bus = events::EventBus::start(
                    &runtime,
                    store.store_state(),
                    app.handle().clone(),
                    Box::new(status),
                );
                store.set_problem_sink(bus.problem_sink());
                // Review A9: writes the revision and safe mode into the
                // bus's view as the baseline, here, under this store lock
                // (nothing sent, nothing waited for); every later change
                // is a `store-state` above it. Commands reach the bus only
                // after `manage` below.
                store.set_store_sink(bus.store_sink());
                bus
            };
            app.manage(bus);
            startup::after_load(app.handle());
            startup::start_engine(engine, runtime);
            // B-10: waits for Healthy, runs the self-test, then listens or
            // stays paused and says why.
            startup::spawn_self_test(app.handle().clone());
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
            commands::settings::set_auto_listen,
            commands::rules::preview_file,
            commands::rules::preview_image,
            commands::system::get_cursor,
            commands::system::capture_selection,
            commands::system::exit_app,
            commands::diagnostics::set_log_level,
            commands::diagnostics::export_diagnostics,
            commands::diagnostics::open_log_dir,
            commands::diagnostics::open_data_dir,
            commands::diagnostics::log_frontend_error,
            commands::diagnostics::report_metric,
            commands::runtime::startup_problems,
            commands::engine::get_runtime_snapshot,
            commands::engine::get_health,
            commands::engine::repair_engine,
            commands::engine::get_capabilities,
            commands::engine::list_history,
            commands::engine::clear_history,
            commands::engine::explain_last_miss,
            commands::engine::self_test,
            #[cfg(feature = "fault-injection")]
            commands::engine::debug_inject_fault
        ])
        .build(tauri::generate_context!())
        .expect("Quicker Text startup failed")
        .run(|app, event| {
            let exiting = matches!(event, tauri::RunEvent::Exit);
            // Clears the run marker (safe mode, B-04) before the exit is logged,
            // so the final flush also covers anything it logs.
            startup::on_run_event(app, event);
            if exiting {
                tracing::info!(event = "app.exit", "Quicker Text exiting");
                diagnostics::logging::flush();
            }
        });
}

// B-52, Codex F3-1: the test-only features never reach an optimized build of
// the app. Nothing in the repository builds it that way (scripts, workflows,
// docs: fault injection and the mock run under `cargo test` and debug builds),
// so any such build is a mistake, caught here before it can be packaged.
// Tests and debug builds keep both (`debug_assertions` is on).
#[cfg(all(
    not(debug_assertions),
    any(feature = "fault-injection", feature = "mock")
))]
compile_error!(
    "the `fault-injection` and `mock` features are for tests and debug builds only (B-52); \
     build the release app without them, or use a profile with debug-assertions = true"
);

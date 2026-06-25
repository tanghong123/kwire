//! Tauri v2 command layer wrapping the `libgen-core` engine.
//!
//! The front end (the static UI under `app/ui`) drives the real
//! parse → persist → query → match → plan → download pipeline through these
//! commands, and listens for `download://progress` events to render live
//! download progress. No UI code touches the network or DB directly — every
//! command goes through the engine's [`Orchestrator`], exactly as the CLI's
//! `run-list` harness does.

// `commands` is `pub` so the integration tests under `tests/` (which compile
// against the crate's public API) can construct an `AppState`, load lists, set
// goals, and drive the engine headlessly — exercising the per-orchestrator
// locking + the goal-driven driver exactly as the Tauri commands do. It is not
// a stable external API; the front end only ever calls the registered commands.
#[doc(hidden)]
pub mod commands;

// Tauri-only engine glue: TauriEmitter + spawn(AppHandle).
mod engine_tauri;

// Re-export the moved modules so existing `crate::bridge`, `crate::state`,
// `crate::viewmodel`, `crate::engine` paths in commands.rs still resolve.
// (Commands that previously used `crate::state::AppState` etc. now get them
// from libgen_engine via these re-exports.)
#[doc(hidden)]
pub use libgen_engine::bridge;
#[doc(hidden)]
pub use libgen_engine::engine;
#[doc(hidden)]
pub use libgen_engine::state;
#[doc(hidden)]
pub use libgen_engine::viewmodel;

use libgen_engine::state::{AppState, Config};

/// Entry point shared by the desktop binary (and, if added later, mobile).
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            // Resume-on-launch: attach to every persisted list in the on-disk DB
            // so a relaunch shows prior state and can continue downloads. The
            // resolved config (incl. the global app settings from
            // `app-config.json`) is also stashed in state so the Settings
            // commands can read/update it at runtime.
            use tauri::Manager;
            let state = app.state::<AppState>();
            let cfg = Config::from_env();
            commands::resume_on_launch(&state, &cfg);
            if let Ok(mut guard) = state.config.lock() {
                *guard = cfg;
            }
            // Start the long-lived execution engine AFTER resume (launch is
            // paused: every book's goal is Idle, so the engine does nothing until
            // a command — Start / Re-query — raises a goal and wakes it).
            engine_tauri::spawn(app.handle().clone());

            // Background: fill in missing book covers (Open Library) off-lock, and
            // cache thumbnails locally so the UI can show them.
            commands::spawn_cover_backfill(
                std::sync::Arc::clone(&state.library),
                std::sync::Arc::clone(&state.config),
            );

            // Background: integrity scan — demote any `Done` variation whose file
            // is missing on disk to `Failed` ("data lost"), then refresh the UI.
            // Off the launch path so it never delays startup.
            commands::spawn_download_verify(
                std::sync::Arc::clone(&state.library),
                app.handle().clone(),
            );

            // Native menu: Settings lives in the app menu (⌘,) instead of the UI
            // toolbar. Selecting it emits `menu://settings` to the front end,
            // which opens the Settings sheet. An Edit menu is included so the
            // standard copy/paste/select-all shortcuts work in text fields.
            use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
            let settings_item =
                MenuItem::with_id(app, "settings", "Settings…", true, Some("CmdOrCtrl+,"))?;
            let app_menu = Submenu::with_items(
                app,
                "Kwire",
                true,
                &[
                    &PredefinedMenuItem::about(app, None, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &settings_item,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::services(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::show_all(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?;
            let edit_menu = Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?;
            let menu = Menu::with_items(app, &[&app_menu, &edit_menu])?;
            app.set_menu(menu)?;
            app.on_menu_event(|app, event| {
                if event.id() == "settings" {
                    use tauri::Emitter;
                    let _ = app.emit("menu://settings", ());
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::parse_preview,
            commands::library,
            commands::select_list,
            commands::load_list,
            commands::add_manual_book,
            commands::remove_book,
            commands::delete_list,
            commands::start,
            commands::stop,
            commands::start_all,
            commands::stop_all,
            commands::query_and_match,
            commands::requery,
            commands::select_candidate,
            commands::mark_not_found,
            commands::request_variation,
            commands::cancel_variation,
            commands::replace_download,
            commands::remove_download,
            commands::pause_variation,
            commands::resume_variation,
            commands::cancel_download,
            commands::pause_all,
            commands::resume_all,
            commands::set_format_pref,
            commands::set_settings,
            commands::get_config,
            commands::set_config,
            commands::refresh_mirrors,
            commands::reorganize_files,
            commands::reorganize_needed,
            commands::reorganize_diff,
            commands::cleanup_part_files,
            commands::add_manual_download,
            commands::accept_download,
            commands::cover_data_url,
            commands::retry,
            commands::edit_book,
            commands::start_downloads,
            commands::download_series,
            commands::reveal,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

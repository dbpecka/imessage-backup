mod commands;
mod core;
mod error;

pub use error::AppError;

use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::Emitter;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let clean_item =
                MenuItemBuilder::with_id("clean-orphans", "Clean Orphaned Data…").build(app)?;

            let file_menu = SubmenuBuilder::new(app, "File").item(&clean_item).build()?;

            // Build a full macOS menu bar: app-name menu, File, Edit.
            // Without this, set_menu() replaces the system default and loses
            // Quit, Cut/Copy/Paste, etc.
            let app_menu = SubmenuBuilder::new(app, "iMessage Backup")
                .about(None)
                .separator()
                .services()
                .separator()
                .hide()
                .hide_others()
                .show_all()
                .separator()
                .quit()
                .build()?;

            let edit_menu = SubmenuBuilder::new(app, "Edit")
                .undo()
                .redo()
                .separator()
                .cut()
                .copy()
                .paste()
                .select_all()
                .build()?;

            let menu = MenuBuilder::new(app)
                .item(&app_menu)
                .item(&file_menu)
                .item(&edit_menu)
                .build()?;

            app.set_menu(menu)?;
            Ok(())
        })
        .on_menu_event(|app, event| {
            if event.id() == "clean-orphans" {
                app.emit("menu:clean-orphans", ()).ok();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::fda::check_fda,
            commands::fda::open_fda_settings,
            commands::fda::relaunch_app,
            commands::discover::probe_db,
            commands::discover::list_chats,
            commands::discover::list_contacts,
            commands::preview::preview_backup,
            commands::backup::run_backup,
            commands::delete::preview_delete,
            commands::delete::run_delete,
            commands::safety::safety_status,
            commands::orphans::scan_orphans,
            commands::orphans::clean_orphans,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

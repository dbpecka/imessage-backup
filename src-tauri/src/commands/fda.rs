use crate::core::db_path::default_chat_db_path;
use crate::error::AppError;
use serde::Serialize;
use std::fs::File;
use std::process::Command;
use tauri::AppHandle;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FdaStatus {
    pub granted: bool,
    pub path: String,
}

/// Cheap read-probe against chat.db to decide whether Full Disk Access has
/// been granted. Opening the file from our Rust process is also what makes
/// the app appear in the FDA list in System Settings, so the user only has
/// to flip the toggle instead of hunting with the `+` button.
#[tauri::command]
pub async fn check_fda() -> Result<FdaStatus, AppError> {
    let path = default_chat_db_path()?;
    let path_str = path.display().to_string();

    if !path.exists() {
        return Err(AppError::Other(format!("chat.db not found at {path_str}")));
    }

    match File::open(&path) {
        Ok(_) => Ok(FdaStatus {
            granted: true,
            path: path_str,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(FdaStatus {
            granted: false,
            path: path_str,
        }),
        Err(e) => Err(AppError::from(e)),
    }
}

/// Deep-link into System Settings → Privacy & Security → Full Disk Access so
/// the user lands on the right pane instead of navigating by hand.
#[tauri::command]
pub fn open_fda_settings() -> Result<(), AppError> {
    Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .spawn()
        .map_err(AppError::from)?;
    Ok(())
}

/// TCC decisions are evaluated at open(2) time, but a process that was denied
/// before the user flipped the toggle may keep serving cached denials until
/// restart. Offering a clean relaunch sidesteps the ambiguity.
#[tauri::command]
pub fn relaunch_app(app: AppHandle) {
    app.restart();
}

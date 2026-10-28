use crate::core::icloud::{detect_icloud_messages, ICloudState};
use crate::core::messages_app::is_messages_running;
use crate::error::AppError;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetyStatus {
    pub messages_running: bool,
    pub icloud_messages: ICloudState,
    /// Where earlier builds kept full copies of chat.db (under
    /// `~/Documents`, which may sync to iCloud Drive), if any are still
    /// there. Shown so the user can remove them; never deleted for them.
    pub legacy_snapshot_dir: Option<String>,
}

fn legacy_snapshot_dir() -> Option<String> {
    let dir = crate::core::db_path::home_dir()
        .ok()?
        .join("Documents/iMessage Backups/snapshots");
    std::fs::read_dir(&dir)
        .ok()?
        .next()
        .is_some()
        .then(|| dir.display().to_string())
}

/// Collect the safety signals the frontend needs to decide whether to allow
/// a delete operation to proceed. Never performs any mutation.
#[tauri::command]
pub async fn safety_status() -> Result<SafetyStatus, AppError> {
    // Same conservative default as `run_delete`: if we can't tell, report
    // "running" so the UI doesn't show a delete as safe that the backend
    // will then refuse.
    let messages_running = is_messages_running().unwrap_or(true);
    let icloud_messages = detect_icloud_messages().unwrap_or(ICloudState::Unknown);
    Ok(SafetyStatus {
        messages_running,
        icloud_messages,
        legacy_snapshot_dir: legacy_snapshot_dir(),
    })
}

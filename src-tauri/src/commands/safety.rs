use crate::core::icloud::{detect_icloud_messages, ICloudState};
use crate::core::messages_app::is_messages_running;
use crate::error::AppError;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetyStatus {
    pub messages_running: bool,
    pub icloud_messages: ICloudState,
}

/// Collect the safety signals the frontend needs to decide whether to allow
/// a delete operation to proceed. Never performs any mutation.
#[tauri::command]
pub async fn safety_status() -> Result<SafetyStatus, AppError> {
    let messages_running = is_messages_running().unwrap_or(false);
    let icloud_messages = detect_icloud_messages().unwrap_or(ICloudState::Unknown);
    Ok(SafetyStatus {
        messages_running,
        icloud_messages,
    })
}

use crate::error::AppError;
use std::path::PathBuf;

/// Default macOS chat.db location: `~/Library/Messages/chat.db`.
pub fn default_chat_db_path() -> Result<PathBuf, AppError> {
    let home = std::env::var("HOME")
        .map_err(|_| AppError::Other("HOME environment variable is not set".into()))?;
    Ok(PathBuf::from(home).join("Library/Messages/chat.db"))
}

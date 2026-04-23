use serde::Serialize;
use thiserror::Error;

/// App-level errors surfaced to the Tauri frontend. Serialised as a tagged
/// JSON enum (`{"kind": "...", "data": ...}`) so the frontend can branch on
/// kind without regex-matching message strings.
#[derive(Debug, Error, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "camelCase")]
pub enum AppError {
    #[error("database error: {0}")]
    Database(String),

    #[error("full disk access not granted for {path}")]
    FullDiskAccess { path: String },

    #[error("io error: {0}")]
    Io(String),

    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        AppError::Io(value.to_string())
    }
}

impl From<imessage_database::error::table::TableError> for AppError {
    fn from(value: imessage_database::error::table::TableError) -> Self {
        AppError::Database(value.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(value: rusqlite::Error) -> Self {
        AppError::Database(value.to_string())
    }
}

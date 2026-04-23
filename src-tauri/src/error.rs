use serde::Serialize;
use thiserror::Error;

/// App-level errors surfaced to the Tauri frontend. Serde-serializable so
/// `Result<T, AppError>` works as a `#[tauri::command]` return type.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Database(String),

    #[error("full disk access not granted for {path}")]
    FullDiskAccess { path: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
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

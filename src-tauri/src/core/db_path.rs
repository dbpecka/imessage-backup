use std::path::{Path, PathBuf};
use std::time::Duration;

use imessage_database::tables::table::{get_connection, get_writable_connection};
use rusqlite::{Connection, TransactionBehavior};

use crate::error::AppError;

/// How long a write waits for a lock held by Messages' background
/// processes before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
thread_local! {
    /// Per-thread stand-in for `$HOME`, so end-to-end tests can run against
    /// a throwaway Messages folder in parallel without touching the real
    /// one (or each other).
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Point [`home_dir`] at `dir` for the current thread (tests only).
#[cfg(test)]
pub fn set_test_home(dir: Option<PathBuf>) {
    TEST_HOME.with(|h| *h.borrow_mut() = dir);
}

/// The user's home folder. Every path the app derives from it (chat.db,
/// attachment folders, path validation) goes through here.
pub fn home_dir() -> Result<PathBuf, AppError> {
    #[cfg(test)]
    if let Some(dir) = TEST_HOME.with(|h| h.borrow().clone()) {
        return Ok(dir);
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| AppError::Other("HOME environment variable is not set".into()))
}

/// Default macOS chat.db location: `~/Library/Messages/chat.db`.
pub fn default_chat_db_path() -> Result<PathBuf, AppError> {
    Ok(home_dir()?.join("Library/Messages/chat.db"))
}

/// Check that `path` is reachable, telling a Full Disk Access denial apart
/// from a missing file. `Path::exists()` (which the library uses) reports a
/// TCC denial as "not found".
pub fn check_chat_db_access(path: &Path) -> Result<(), AppError> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(AppError::FullDiskAccess {
                path: path.display().to_string(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AppError::Other(format!(
            "chat.db not found at {}",
            path.display()
        ))),
        Err(e) => Err(AppError::from(e)),
    }
}

/// Open `path` read-only, with Full Disk Access errors reported as such.
pub fn open_read(path: &Path) -> Result<Connection, AppError> {
    check_chat_db_access(path)?;
    Ok(get_connection(path)?)
}

/// Open `path` for writing. Waits up to [`BUSY_TIMEOUT`] for locks, and
/// makes `transaction()` take the write lock up front (`BEGIN IMMEDIATE`)
/// so a delete can't fail halfway through on a lock upgrade.
pub fn open_write(path: &Path) -> Result<Connection, AppError> {
    check_chat_db_access(path)?;
    let mut conn = get_writable_connection(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.set_transaction_behavior(TransactionBehavior::Immediate);
    Ok(conn)
}

/// Run blocking work (SQLite, filesystem walks) off the async runtime.
pub async fn blocking<T, F>(f: F) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Other(format!("background task failed: {e}")))?
}

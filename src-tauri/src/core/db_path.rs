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

/// Whether macOS is refusing this process access to `path` — what happens
/// to everything in `~/Library/Messages` without Full Disk Access. TCC
/// refuses even to stat the file, so a missing file isn't a denial.
pub fn access_denied(path: &Path) -> bool {
    let denied = |e: std::io::Error| e.kind() == std::io::ErrorKind::PermissionDenied;
    match std::fs::metadata(path) {
        Ok(_) => std::fs::File::open(path).err().is_some_and(denied),
        Err(e) => denied(e),
    }
}

/// Report `err` as a Full Disk Access problem if access to chat.db is being
/// refused right now. Access can be revoked while the app is open; the
/// operation then fails with whatever it tripped over first ("Operation not
/// permitted", "unable to open database file", …), which doesn't tell the
/// user what to do.
pub fn explain_access_error(err: AppError) -> AppError {
    if matches!(err, AppError::FullDiskAccess { .. }) {
        return err;
    }
    match default_chat_db_path() {
        Ok(path) if access_denied(&path) => AppError::FullDiskAccess {
            path: path.display().to_string(),
        },
        _ => err,
    }
}

/// Run blocking work (SQLite, filesystem walks) off the async runtime.
/// Failures caused by a revoked Full Disk Access are reported as such.
pub async fn blocking<T, F>(f: F) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || f().map_err(explain_access_error))
        .await
        .map_err(|e| AppError::Other(format!("background task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without Full Disk Access, macOS refuses to stat anything in
    /// `~/Library/Messages`. A folder we can't search stands in for that.
    #[cfg(unix)]
    #[test]
    fn failures_while_access_is_refused_are_reported_as_full_disk_access() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::TempDir::new().unwrap();
        let messages = home.path().join("Library/Messages");
        std::fs::create_dir_all(&messages).unwrap();
        std::fs::write(messages.join("chat.db"), b"").unwrap();
        set_test_home(Some(home.path().to_path_buf()));

        let other = || AppError::Io("Operation not permitted (os error 1)".into());
        // Access granted: errors pass through unchanged.
        assert!(matches!(explain_access_error(other()), AppError::Io(_)));

        std::fs::set_permissions(&messages, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refused = std::fs::metadata(messages.join("chat.db")).is_err();
        let err = explain_access_error(other());
        std::fs::set_permissions(&messages, std::fs::Permissions::from_mode(0o755)).unwrap();
        set_test_home(None);

        // Root ignores folder permissions, so this half only runs as a
        // normal user (as on CI).
        if refused {
            assert!(
                matches!(err, AppError::FullDiskAccess { ref path } if path.ends_with("chat.db")),
                "{err:?}"
            );
        }
    }

    #[test]
    fn a_missing_chat_db_is_not_an_access_problem() {
        let home = tempfile::TempDir::new().unwrap();
        set_test_home(Some(home.path().to_path_buf()));
        let err = explain_access_error(AppError::Other("boom".into()));
        set_test_home(None);
        assert!(matches!(err, AppError::Other(m) if m == "boom"));
    }
}

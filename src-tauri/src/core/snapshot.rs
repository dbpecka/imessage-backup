use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{backup::Backup, Connection, OpenFlags};

use crate::error::AppError;

/// Result of snapshotting `chat.db` to a safety folder before a destructive
/// operation. The caller should surface `dir` in the UI so the user knows
/// where the snapshot landed and how to roll back if needed.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Snapshot {
    pub dir: PathBuf,
    /// Paths actually written. Populated for logging / future display.
    pub copied: Vec<PathBuf>,
}

/// Snapshot `chat.db` atomically into a timestamped folder under
/// `snapshot_root` using SQLite's online backup API. The backup holds a
/// shared lock on the source, so the resulting single-file `chat.db` is a
/// consistent point-in-time image regardless of WAL state — no need to
/// copy `-wal`/`-shm` sidecars or to pre-checkpoint.
pub fn snapshot_chat_db(chat_db: &Path, snapshot_root: &Path) -> Result<Snapshot, AppError> {
    if !chat_db.exists() {
        return Err(AppError::Other(format!(
            "chat.db not found at {}",
            chat_db.display()
        )));
    }

    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let dir = snapshot_root.join(timestamp);
    fs::create_dir_all(&dir)?;

    let db_dest = dir.join("chat.db");

    let src = Connection::open_with_flags(
        chat_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        AppError::Other(format!(
            "failed to open {} for snapshot: {e}",
            chat_db.display()
        ))
    })?;

    let mut dst = Connection::open(&db_dest).map_err(|e| {
        AppError::Other(format!(
            "failed to create snapshot at {}: {e}",
            db_dest.display()
        ))
    })?;

    {
        let backup = Backup::new(&src, &mut dst)
            .map_err(|e| AppError::Other(format!("failed to init online backup: {e}")))?;
        backup
            .run_to_completion(1024, std::time::Duration::from_millis(0), None)
            .map_err(|e| AppError::Other(format!("online backup failed: {e}")))?;
    }

    drop(src);
    drop(dst);

    Ok(Snapshot {
        dir,
        copied: vec![db_dest],
    })
}

/// Run `PRAGMA wal_checkpoint(TRUNCATE)` against `chat_db`. Retained as a
/// defensive pre-step so readers of the *source* database find a clean WAL;
/// the snapshot itself no longer depends on it (we use the SQLite backup
/// API, which is WAL-safe).
pub fn checkpoint_wal(chat_db: &Path) -> Result<(), AppError> {
    use imessage_database::tables::table::get_writable_connection;
    let conn = get_writable_connection(chat_db)
        .map_err(|e| AppError::Other(format!("failed to open for checkpoint: {e}")))?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .map_err(|e| AppError::Other(format!("wal_checkpoint failed: {e}")))?;
    Ok(())
}

/// Default snapshot root under the user's home directory:
/// `~/Documents/iMessage Backups/snapshots`.
pub fn default_snapshot_root() -> Result<PathBuf, AppError> {
    let home = std::env::var("HOME")
        .map_err(|_| AppError::Other("HOME environment variable is not set".into()))?;
    Ok(PathBuf::from(home).join("Documents/iMessage Backups/snapshots"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Build a minimal SQLite db with a single row. Used only in tests —
    /// never touches the real chat.db.
    fn make_fixture_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, text TEXT);
             INSERT INTO message (text) VALUES ('hello'), ('world');",
        )
        .unwrap();
    }

    #[test]
    fn snapshot_round_trips_row_counts() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("chat.db");
        make_fixture_db(&src);

        let snap_root = tmp.path().join("snaps");
        let snap = snapshot_chat_db(&src, &snap_root).unwrap();

        let snap_db = snap.dir.join("chat.db");
        assert!(snap_db.exists(), "snapshot db file should exist");

        let conn = Connection::open(&snap_db).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn snapshot_is_isolated_from_source_mutations() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("chat.db");
        make_fixture_db(&src);

        let snap = snapshot_chat_db(&src, &tmp.path().join("snaps")).unwrap();

        // Mutate the source after snapshot — snapshot must not change.
        let w = Connection::open(&src).unwrap();
        w.execute("DELETE FROM message", []).unwrap();
        drop(w);

        let conn = Connection::open(snap.dir.join("chat.db")).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "snapshot should preserve the pre-delete state");
    }

    #[test]
    fn snapshot_missing_source_errors() {
        let tmp = TempDir::new().unwrap();
        let err = snapshot_chat_db(&tmp.path().join("nope.db"), tmp.path()).unwrap_err();
        match err {
            AppError::Other(m) => assert!(m.contains("not found")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

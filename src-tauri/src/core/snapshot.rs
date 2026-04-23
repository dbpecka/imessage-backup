use std::{
    fs,
    path::{Path, PathBuf},
};

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

/// Copy `chat.db` plus its `-wal` and `-shm` sidecars into a timestamped
/// folder under `snapshot_root`. The `.db` file itself is required to exist;
/// the sidecars are copied only if present (they may be absent if the WAL
/// was checkpointed recently or if journaling isn't WAL mode).
///
/// Does **not** checkpoint the WAL. Call
/// [`checkpoint_wal`](checkpoint_wal) against the source DB first if you
/// want the snapshot to be self-contained.
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

    let mut copied = Vec::new();

    // Main database file — mandatory.
    let db_dest = dir.join("chat.db");
    fs::copy(chat_db, &db_dest).map_err(|e| {
        AppError::Other(format!(
            "failed to copy {} to {}: {e}",
            chat_db.display(),
            db_dest.display()
        ))
    })?;
    copied.push(db_dest);

    // Sidecars — best-effort.
    for sidecar in ["chat.db-wal", "chat.db-shm"] {
        let src = chat_db.with_file_name(sidecar);
        if src.exists() {
            let dst = dir.join(sidecar);
            if let Err(e) = fs::copy(&src, &dst) {
                // Don't fail the whole snapshot for a sidecar; surface it
                // by leaving the file out of `copied`.
                eprintln!("warning: failed to copy {}: {e}", src.display());
            } else {
                copied.push(dst);
            }
        }
    }

    Ok(Snapshot { dir, copied })
}

/// Run `PRAGMA wal_checkpoint(TRUNCATE)` against `chat_db` so a subsequent
/// file-copy snapshot includes all committed data. Opens its own read/write
/// connection for this single operation and closes it immediately.
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

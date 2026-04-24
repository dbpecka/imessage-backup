use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use imessage_database::tables::table::{get_connection, get_writable_connection};
use serde::Serialize;

use crate::core::db_path::default_chat_db_path;
use crate::error::AppError;

const ATTACHMENT_DIRS: &[&str] = &[
    "~/Library/Messages/Attachments",
    "~/Library/Messages/StickerCache",
];

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanScan {
    pub db_orphan_count: u64,
    pub db_orphan_bytes: u64,
    pub fs_orphan_count: u64,
    pub fs_orphan_bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanCleanResult {
    pub db_rows_deleted: u64,
    pub db_files_removed: u64,
    pub db_files_failed: u64,
    pub fs_files_removed: u64,
    pub fs_files_failed: u64,
}

/// Expand a leading `~` to the user's home directory, matching the library's
/// own `gen_macos_attachment` behaviour.
fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(path.replacen('~', &home, 1));
        }
    }
    PathBuf::from(path)
}

fn walk_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Scan for two categories of orphaned data:
///
/// - **DB orphans**: rows in `attachment` with no `message_attachment_join`
///   entry (the attachment is not linked to any message).
/// - **FS orphans**: files under the Messages Attachments/StickerCache
///   directories that have no corresponding row in `attachment` at all.
#[tauri::command]
pub async fn scan_orphans() -> Result<OrphanScan, AppError> {
    let db_path = default_chat_db_path()?;
    let conn = get_connection(&db_path)?;

    // DB orphans -------------------------------------------------------
    // Prefer actual on-disk size; total_bytes in the DB is often 0 or NULL.
    let mut stmt = conn.prepare(
        "SELECT a.filename, COALESCE(a.total_bytes, 0) \
         FROM attachment a \
         LEFT JOIN message_attachment_join j ON j.attachment_id = a.ROWID \
         WHERE j.message_id IS NULL",
    )?;
    let mut db_orphan_count = 0u64;
    let mut db_orphan_bytes = 0u64;
    let db_rows = stmt.query_map([], |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in db_rows.flatten() {
        let (filename, db_bytes) = row;
        db_orphan_count += 1;
        let bytes = filename
            .as_deref()
            .map(expand_tilde)
            .and_then(|p| fs::metadata(&p).ok())
            .map(|m| m.len())
            .unwrap_or(db_bytes.max(0) as u64);
        db_orphan_bytes = db_orphan_bytes.saturating_add(bytes);
    }

    // Known paths (ALL attachment rows, orphaned or not) ---------------
    let mut path_stmt =
        conn.prepare("SELECT filename FROM attachment WHERE filename IS NOT NULL")?;
    let known_paths: HashSet<PathBuf> = path_stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .flatten()
        .map(|p| expand_tilde(&p))
        .collect();

    // FS orphans -------------------------------------------------------
    let mut fs_orphan_count = 0u64;
    let mut fs_orphan_bytes = 0u64;
    for dir_str in ATTACHMENT_DIRS {
        let dir = expand_tilde(dir_str);
        if !dir.exists() {
            continue;
        }
        let mut files = Vec::new();
        walk_files(&dir, &mut files);
        for path in files {
            if !known_paths.contains(&path) {
                fs_orphan_count += 1;
                fs_orphan_bytes = fs_orphan_bytes
                    .saturating_add(fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
            }
        }
    }

    Ok(OrphanScan {
        db_orphan_count,
        db_orphan_bytes,
        fs_orphan_count,
        fs_orphan_bytes,
    })
}

/// Delete all orphaned data found by [`scan_orphans`].
///
/// Execution order:
/// 1. Collect orphan rowids + paths with read connection (then drop it).
/// 2. Delete DB orphan rows via a writable transaction.
/// 3. Remove DB orphan files from disk.
/// 4. Remove FS orphan files from disk.
#[tauri::command]
pub async fn clean_orphans() -> Result<OrphanCleanResult, AppError> {
    let db_path = default_chat_db_path()?;

    // Phase 1: collect everything while holding only a read connection.
    let (db_orphan_rowids, db_orphan_paths, fs_orphan_paths) = {
        let conn = get_connection(&db_path)?;

        let mut stmt = conn.prepare(
            "SELECT a.ROWID, a.filename \
             FROM attachment a \
             LEFT JOIN message_attachment_join j ON j.attachment_id = a.ROWID \
             WHERE j.message_id IS NULL",
        )?;
        let pairs: Vec<(i64, Option<String>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .flatten()
            .collect();

        let db_orphan_rowids: Vec<i64> = pairs.iter().map(|(id, _)| *id).collect();
        let db_orphan_paths: Vec<Option<PathBuf>> = pairs
            .iter()
            .map(|(_, f)| f.as_deref().map(expand_tilde))
            .collect();

        let mut path_stmt =
            conn.prepare("SELECT filename FROM attachment WHERE filename IS NOT NULL")?;
        let known_paths: HashSet<PathBuf> = path_stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .flatten()
            .map(|p| expand_tilde(&p))
            .collect();

        let mut fs_orphan_paths: Vec<PathBuf> = Vec::new();
        for dir_str in ATTACHMENT_DIRS {
            let dir = expand_tilde(dir_str);
            if !dir.exists() {
                continue;
            }
            let mut files = Vec::new();
            walk_files(&dir, &mut files);
            for path in files {
                if !known_paths.contains(&path) {
                    fs_orphan_paths.push(path);
                }
            }
        }

        (db_orphan_rowids, db_orphan_paths, fs_orphan_paths)
        // conn dropped here
    };

    // Phase 2: delete DB orphan rows in a single transaction.
    let mut db_rows_deleted = 0u64;
    if !db_orphan_rowids.is_empty() {
        let mut conn = get_writable_connection(&db_path)?;
        let tx = conn.transaction()?;
        for &rowid in &db_orphan_rowids {
            if tx.execute(
                "DELETE FROM attachment WHERE ROWID = ?1",
                rusqlite::params![rowid],
            )? > 0
            {
                db_rows_deleted += 1;
            }
        }
        tx.commit()?;
    }

    // Phase 3: remove DB orphan files from disk.
    let mut db_files_removed = 0u64;
    let mut db_files_failed = 0u64;
    for path in db_orphan_paths.iter().flatten() {
        if path.exists() {
            match fs::remove_file(path) {
                Ok(()) => db_files_removed += 1,
                Err(e) => {
                    eprintln!(
                        "orphan-clean: failed to remove db-orphan file {}: {e}",
                        path.display()
                    );
                    db_files_failed += 1;
                }
            }
        }
    }

    // Phase 4: remove FS orphan files.
    let mut fs_files_removed = 0u64;
    let mut fs_files_failed = 0u64;
    for path in &fs_orphan_paths {
        match fs::remove_file(path) {
            Ok(()) => fs_files_removed += 1,
            Err(e) => {
                eprintln!(
                    "orphan-clean: failed to remove fs-orphan file {}: {e}",
                    path.display()
                );
                fs_files_failed += 1;
            }
        }
    }

    Ok(OrphanCleanResult {
        db_rows_deleted,
        db_files_removed,
        db_files_failed,
        fs_files_removed,
        fs_files_failed,
    })
}

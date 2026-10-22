use std::collections::{BTreeSet, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use imessage_database::tables::table::{get_connection, get_writable_connection};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::core::attachment_files::{
    attachment_roots, expand_tilde, is_within_roots, path_key, remove_attachment_file, walk_files,
    RemoveOutcome,
};
use crate::core::db_path::default_chat_db_path;
use crate::core::messages_app::is_messages_running;
use crate::core::retained::RetainedAttachments;
use crate::core::snapshot::{checkpoint_wal, default_snapshot_root, snapshot_chat_db};
use crate::error::AppError;

/// Files newer than this are never treated as filesystem orphans: Messages
/// can write an attachment's file before the row that references it is
/// committed (e.g. an in-flight download).
const MIN_FS_ORPHAN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanScan {
    pub db_orphan_count: u64,
    pub db_orphan_bytes: u64,
    pub fs_orphan_count: u64,
    pub fs_orphan_bytes: u64,
    /// Identifies the exact set of orphans found. `clean_orphans` refuses to
    /// run unless it finds the same set, so it can only ever delete what the
    /// user reviewed.
    pub fingerprint: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanOrphansArgs {
    /// `fingerprint` from the `scan_orphans` result the user confirmed.
    pub scan_fingerprint: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanCleanResult {
    pub db_rows_deleted: u64,
    pub db_files_removed: u64,
    pub db_files_failed: u64,
    pub fs_files_removed: u64,
    pub fs_files_failed: u64,
    /// Files left alone because they weren't regular files inside the
    /// Messages attachment folders.
    pub files_skipped: u64,
    /// Snapshot of chat.db taken before deleting rows; `None` when no rows
    /// were deleted.
    pub snapshot_path: Option<String>,
}

#[derive(Debug)]
struct DbOrphan {
    rowid: i64,
    /// File to unlink with the row. `None` when the row has no file, the
    /// file is shared with a row that's being kept, or it lies outside the
    /// Messages attachment folders.
    path: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct Orphans {
    db: Vec<DbOrphan>,
    db_bytes: u64,
    fs: Vec<PathBuf>,
    fs_bytes: u64,
}

impl Orphans {
    fn fingerprint(&self) -> String {
        let mut h = DefaultHasher::new();
        for o in &self.db {
            o.rowid.hash(&mut h);
            o.path.hash(&mut h);
        }
        self.fs.hash(&mut h);
        format!("{}:{}:{:016x}", self.db.len(), self.fs.len(), h.finish())
    }
}

/// `true` if `path` sits in a directory (below `root`) that also holds a
/// file referenced by chat.db. Messages keeps companion files next to the
/// referenced one (Live Photo videos, previews, …) that have no row of
/// their own, so we only ever treat whole unreferenced folders as orphaned.
fn in_referenced_dir(path: &Path, root: &Path, referenced_dirs: &HashSet<String>) -> bool {
    let root_key = path_key(root);
    let mut dir = path.parent();
    while let Some(d) = dir {
        let key = path_key(d);
        if key.len() <= root_key.len() {
            break;
        }
        if referenced_dirs.contains(&key) {
            return true;
        }
        dir = d.parent();
    }
    false
}

fn is_recent(path: &Path, now: SystemTime) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        // Can't tell how old it is — don't risk it.
        return true;
    };
    match now.duration_since(modified) {
        Ok(age) => age < MIN_FS_ORPHAN_AGE,
        // Modified "in the future" (clock skew): treat as recent.
        Err(_) => true,
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Find two categories of orphaned data:
///
/// - **DB orphans**: rows in `attachment` with no `message_attachment_join`
///   entry, excluding attachments the user kept via a messages-only delete.
/// - **FS orphans**: files under the Messages Attachments/StickerCache
///   directories whose folder holds no file referenced by any `attachment`
///   row, and that are at least [`MIN_FS_ORPHAN_AGE`] old.
fn find_orphans(
    conn: &Connection,
    retained: &BTreeSet<i64>,
    roots: &[PathBuf],
    now: SystemTime,
) -> Result<Orphans, AppError> {
    let mut stmt = conn.prepare(
        "SELECT a.ROWID, a.filename, \
                EXISTS (SELECT 1 FROM message_attachment_join j WHERE j.attachment_id = a.ROWID) \
         FROM attachment a \
         ORDER BY a.ROWID",
    )?;
    let rows: Vec<(i64, Option<String>, bool)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;

    let mut referenced_dirs: HashSet<String> = HashSet::new();
    let mut kept_files: HashSet<String> = HashSet::new();
    let mut candidates: Vec<(i64, Option<PathBuf>)> = Vec::new();

    for (rowid, filename, linked) in rows {
        let path = filename.as_deref().map(expand_tilde);
        if let Some(p) = &path {
            if let Some(parent) = p.parent() {
                referenced_dirs.insert(path_key(parent));
            }
        }
        if linked || retained.contains(&rowid) {
            if let Some(p) = &path {
                kept_files.insert(path_key(p));
            }
        } else {
            candidates.push((rowid, path));
        }
    }

    let mut out = Orphans::default();
    for (rowid, path) in candidates {
        let path = path.filter(|p| is_within_roots(p, roots) && !kept_files.contains(&path_key(p)));
        if let Some(p) = &path {
            out.db_bytes = out.db_bytes.saturating_add(file_len(p));
        }
        out.db.push(DbOrphan { rowid, path });
    }

    for root in roots {
        let mut files = Vec::new();
        walk_files(root, &mut files);
        for path in files {
            if in_referenced_dir(&path, root, &referenced_dirs) || is_recent(&path, now) {
                continue;
            }
            out.fs_bytes = out.fs_bytes.saturating_add(file_len(&path));
            out.fs.push(path);
        }
    }
    out.fs.sort();

    Ok(out)
}

#[tauri::command]
pub async fn scan_orphans(app: AppHandle) -> Result<OrphanScan, AppError> {
    let db_path = default_chat_db_path()?;
    let conn = get_connection(&db_path)?;
    let retained = RetainedAttachments::for_app(&app)?.load()?;
    let orphans = find_orphans(&conn, &retained, &attachment_roots(), SystemTime::now())?;

    Ok(OrphanScan {
        db_orphan_count: orphans.db.len() as u64,
        db_orphan_bytes: orphans.db_bytes,
        fs_orphan_count: orphans.fs.len() as u64,
        fs_orphan_bytes: orphans.fs_bytes,
        fingerprint: orphans.fingerprint(),
    })
}

fn ensure_messages_not_running() -> Result<(), AppError> {
    if is_messages_running().unwrap_or(true) {
        return Err(AppError::Other(
            "Quit Messages.app before cleaning orphaned data — it may be writing new \
             attachments."
                .into(),
        ));
    }
    Ok(())
}

/// Delete the orphaned data found by [`scan_orphans`].
///
/// Execution order:
/// 1. Refuse if Messages.app is running.
/// 2. Recompute orphans and refuse unless they match the reviewed scan.
/// 3. If rows will be deleted: WAL checkpoint + chat.db snapshot.
/// 4. Delete DB orphan rows via a writable transaction.
/// 5. Remove DB orphan files, then FS orphan files, from disk.
#[tauri::command]
pub async fn clean_orphans(
    app: AppHandle,
    args: CleanOrphansArgs,
) -> Result<OrphanCleanResult, AppError> {
    ensure_messages_not_running()?;

    let db_path = default_chat_db_path()?;
    let roots = attachment_roots();
    let retained = RetainedAttachments::for_app(&app)?.load()?;

    let orphans = {
        let conn = get_connection(&db_path)?;
        find_orphans(&conn, &retained, &roots, SystemTime::now())?
        // conn dropped here
    };
    if orphans.fingerprint() != args.scan_fingerprint {
        return Err(AppError::Other(
            "Orphaned data changed since the scan. Run the scan again and review the new \
             results before cleaning."
                .into(),
        ));
    }

    let mut db_rows_deleted = 0u64;
    let mut snapshot_path = None;
    if !orphans.db.is_empty() {
        checkpoint_wal(&db_path)?;
        let snapshot = snapshot_chat_db(&db_path, &default_snapshot_root()?)?;
        snapshot_path = Some(snapshot.dir.display().to_string());

        ensure_messages_not_running()?;
        let mut conn = get_writable_connection(&db_path)?;
        let tx = conn.transaction()?;
        for o in &orphans.db {
            // Re-check the orphan condition inside the transaction so a row
            // that gained a link since the scan is left alone.
            if tx.execute(
                "DELETE FROM attachment WHERE ROWID = ?1 \
                 AND NOT EXISTS (SELECT 1 FROM message_attachment_join WHERE attachment_id = ?1)",
                rusqlite::params![o.rowid],
            )? > 0
            {
                db_rows_deleted += 1;
            }
        }
        tx.commit()?;
    }

    let mut files_skipped = 0u64;
    let mut db_files_removed = 0u64;
    let mut db_files_failed = 0u64;
    for path in orphans.db.iter().filter_map(|o| o.path.as_deref()) {
        match remove_attachment_file(path, &roots, "orphan-clean") {
            RemoveOutcome::Removed => db_files_removed += 1,
            RemoveOutcome::Missing => {}
            RemoveOutcome::Refused => files_skipped += 1,
            RemoveOutcome::Failed => db_files_failed += 1,
        }
    }

    let mut fs_files_removed = 0u64;
    let mut fs_files_failed = 0u64;
    for path in &orphans.fs {
        match remove_attachment_file(path, &roots, "orphan-clean") {
            RemoveOutcome::Removed => fs_files_removed += 1,
            RemoveOutcome::Missing => {}
            RemoveOutcome::Refused => files_skipped += 1,
            RemoveOutcome::Failed => fs_files_failed += 1,
        }
    }

    Ok(OrphanCleanResult {
        db_rows_deleted,
        db_files_removed,
        db_files_failed,
        fs_files_removed,
        fs_files_failed,
        files_skipped,
        snapshot_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fixture(tmp: &TempDir) -> (Connection, PathBuf) {
        let root = tmp.path().join("Attachments");
        fs::create_dir_all(&root).unwrap();
        let conn = Connection::open(tmp.path().join("chat.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY, filename TEXT);
             CREATE TABLE message_attachment_join (message_id INTEGER, attachment_id INTEGER);",
        )
        .unwrap();
        (conn, root)
    }

    fn file(path: &Path) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"data").unwrap();
        path.to_path_buf()
    }

    fn add_row(conn: &Connection, rowid: i64, path: &Path, linked: bool) {
        conn.execute(
            "INSERT INTO attachment (ROWID, filename) VALUES (?1, ?2)",
            rusqlite::params![rowid, path.to_str().unwrap()],
        )
        .unwrap();
        if linked {
            conn.execute(
                "INSERT INTO message_attachment_join VALUES (1, ?1)",
                rusqlite::params![rowid],
            )
            .unwrap();
        }
    }

    fn later() -> SystemTime {
        SystemTime::now() + MIN_FS_ORPHAN_AGE + Duration::from_secs(60)
    }

    #[test]
    fn companion_files_in_referenced_folders_are_kept() {
        let tmp = TempDir::new().unwrap();
        let (conn, root) = fixture(&tmp);
        let heic = file(&root.join("ab/01/GUID-1/IMG_1.HEIC"));
        file(&root.join("ab/01/GUID-1/IMG_1.MOV"));
        file(&root.join("ab/01/GUID-1/sub/preview.png"));
        let stray = file(&root.join("cd/02/GUID-2/old.jpg"));
        add_row(&conn, 1, &heic, true);

        let o = find_orphans(&conn, &BTreeSet::new(), &[root], later()).unwrap();
        assert!(o.db.is_empty());
        assert_eq!(o.fs, vec![stray]);
    }

    #[test]
    fn path_case_differences_do_not_create_orphans() {
        let tmp = TempDir::new().unwrap();
        let (conn, root) = fixture(&tmp);
        let real = file(&root.join("ab/01/GUID-1/a.jpg"));
        let db_spelling = PathBuf::from(real.to_str().unwrap().replace("GUID-1", "guid-1"));
        add_row(&conn, 1, &db_spelling, true);

        let o = find_orphans(&conn, &BTreeSet::new(), &[root], later()).unwrap();
        assert!(o.fs.is_empty());
    }

    #[test]
    fn recent_files_are_not_fs_orphans() {
        let tmp = TempDir::new().unwrap();
        let (conn, root) = fixture(&tmp);
        file(&root.join("ab/01/GUID-NEW/incoming.jpg"));

        let o = find_orphans(&conn, &BTreeSet::new(), &[root], SystemTime::now()).unwrap();
        assert!(o.fs.is_empty());
    }

    #[test]
    fn db_orphans_respect_retained_shared_and_out_of_root_files() {
        let tmp = TempDir::new().unwrap();
        let (conn, root) = fixture(&tmp);
        let shared = file(&root.join("ab/01/G/shared.jpg"));
        let own = file(&root.join("ab/02/G/own.jpg"));
        let outside = file(&tmp.path().join("Documents/important.pdf"));
        add_row(&conn, 1, &shared, true);
        add_row(&conn, 2, &shared, false); // orphan row sharing a kept file
        add_row(&conn, 3, &own, false); // genuine orphan
        add_row(&conn, 4, &outside, false); // orphan row pointing outside
        add_row(&conn, 5, &own, false);
        let solo = file(&root.join("ab/03/G/solo.jpg"));
        add_row(&conn, 6, &solo, false); // orphan with its own file
        let retained = BTreeSet::from([5]);

        let o = find_orphans(&conn, &retained, &[root], later()).unwrap();
        let got: Vec<(i64, Option<PathBuf>)> =
            o.db.iter().map(|d| (d.rowid, d.path.clone())).collect();
        // Row 5 is retained, so its file (shared with orphan row 3) is kept.
        assert_eq!(got, vec![(2, None), (3, None), (4, None), (6, Some(solo))]);
    }

    #[test]
    fn fingerprint_changes_when_orphans_change() {
        let tmp = TempDir::new().unwrap();
        let (conn, root) = fixture(&tmp);
        let a = file(&root.join("ab/01/G/a.jpg"));
        add_row(&conn, 1, &a, false);
        let roots = [root];
        let f1 = find_orphans(&conn, &BTreeSet::new(), &roots, later())
            .unwrap()
            .fingerprint();
        let f2 = find_orphans(&conn, &BTreeSet::new(), &roots, later())
            .unwrap()
            .fingerprint();
        assert_eq!(f1, f2);

        add_row(&conn, 2, &a, false);
        let f3 = find_orphans(&conn, &BTreeSet::new(), &roots, later())
            .unwrap()
            .fingerprint();
        assert_ne!(f1, f3);
    }
}

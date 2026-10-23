use std::collections::{BTreeSet, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::commands::delete::SafetyProbe;
use crate::core::attachment_files::{
    attachment_roots, expand_tilde, is_within_roots, path_key, remove_attachment_file, walk_files,
    RemoveOutcome,
};
use crate::core::db_path::{blocking, default_chat_db_path, open_read, open_write};
use crate::core::icloud::ICloudState;
use crate::core::op_lock::OperationLock;
use crate::core::retained::RetainedAttachments;
use crate::core::snapshot::{
    checkpoint_wal, default_snapshot_root, prune_snapshots, snapshot_chat_db, SnapshotKind,
    SNAPSHOTS_TO_KEEP,
};
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
    /// Unlinked attachments the user kept via a messages-only delete. Not
    /// included in the counts above unless the scan asked for them.
    pub retained_count: u64,
    pub retained_bytes: u64,
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
    /// Also remove attachments kept by earlier messages-only deletes. Must
    /// match the `include_retained` the scan ran with.
    #[serde(default)]
    pub include_retained: bool,
    /// Required (when records will be deleted) unless Messages in iCloud
    /// is positively detected as disabled: chat.db's triggers queue every
    /// deleted attachment record for iCloud deletion.
    #[serde(default)]
    pub acknowledge_icloud_sync: bool,
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
    /// Retained (kept) unlinked attachments left out of `db`.
    retained_count: u64,
    retained_bytes: u64,
    /// Retained ids included in `db` because the caller asked for them.
    included_retained: Vec<i64>,
    /// Retained ids whose attachment record no longer exists and can be
    /// dropped from the list.
    stale_retained: Vec<i64>,
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

/// Resolve how chat.db spells a path into how the filesystem walk will see
/// it, so the same file can't look unreferenced because of spelling:
/// - rebuild from components (drops `//` and `.`),
/// - resolve symlinks in the parent folder (e.g. an old home path left as a
///   link after the home folder moved),
/// - if it still isn't under an attachment root but clearly belongs to one
///   (`…/Library/Messages/Attachments/…` under some other home), map its
///   tail onto the current root.
fn normalize_stored_path(path: &Path, roots: &[PathBuf]) -> PathBuf {
    let rebuilt: PathBuf = path.components().collect();
    let resolved = match (rebuilt.parent(), rebuilt.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|p| p.join(name))
            .unwrap_or_else(|_| rebuilt.clone()),
        _ => rebuilt.clone(),
    };
    if is_within_roots(&resolved, roots) {
        return resolved;
    }
    let text = resolved.to_string_lossy();
    let lower = text.to_lowercase();
    for root in roots {
        let Some(name) = root.file_name() else {
            continue;
        };
        let marker = format!(
            "/library/messages/{}/",
            name.to_string_lossy().to_lowercase()
        );
        if let Some(i) = lower.rfind(&marker) {
            let candidate = root.join(&text[i + marker.len()..]);
            if is_within_roots(&candidate, roots) {
                return candidate;
            }
        }
    }
    resolved
}

/// chat.db `created_date`: seconds since 2001, or nanoseconds on newer
/// macOS. `None` when unset.
fn created_at(apple_date: i64) -> Option<SystemTime> {
    if apple_date <= 0 {
        return None;
    }
    let secs = if apple_date > 100_000_000_000 {
        apple_date / 1_000_000_000
    } else {
        apple_date
    };
    let unix = u64::try_from(secs).ok()? + 978_307_200;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(unix))
}

/// Find two categories of orphaned data:
///
/// - **DB orphans**: rows in `attachment` with no `message_attachment_join`
///   entry, excluding attachments the user kept via a messages-only delete
///   and records created in the last [`MIN_FS_ORPHAN_AGE`] (their message
///   may simply not have synced to this Mac yet).
/// - **FS orphans**: files under the Messages Attachments/StickerCache
///   directories whose folder holds no file referenced by any `attachment`
///   row, and that are at least [`MIN_FS_ORPHAN_AGE`] old.
fn find_orphans(
    conn: &Connection,
    retained: &BTreeSet<i64>,
    include_retained: bool,
    roots: &[PathBuf],
    now: SystemTime,
) -> Result<Orphans, AppError> {
    let mut stmt = conn.prepare(
        "SELECT a.ROWID, a.filename, \
                EXISTS (SELECT 1 FROM message_attachment_join j WHERE j.attachment_id = a.ROWID), \
                COALESCE(a.created_date, 0) \
         FROM attachment a \
         ORDER BY a.ROWID",
    )?;
    let rows: Vec<(i64, Option<String>, bool, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;

    let mut referenced_dirs: HashSet<String> = HashSet::new();
    // Every file any record points at. Never a filesystem orphan, even when
    // it sits directly in a root folder (where folder matching stops).
    let mut referenced_files: HashSet<String> = HashSet::new();
    let mut kept_files: HashSet<String> = HashSet::new();
    let mut candidates: Vec<(i64, Option<PathBuf>)> = Vec::new();
    let mut existing: HashSet<i64> = HashSet::new();
    let mut out = Orphans::default();

    for (rowid, filename, linked, created) in rows {
        let path = filename
            .as_deref()
            .map(|f| normalize_stored_path(&expand_tilde(f), roots));
        if let Some(p) = &path {
            referenced_files.insert(path_key(p));
            if let Some(parent) = p.parent() {
                referenced_dirs.insert(path_key(parent));
            }
        }
        existing.insert(rowid);
        let is_retained = !linked && retained.contains(&rowid);
        // Unset dates count as old; a date in the future (clock skew)
        // counts as new.
        let too_new = created_at(created).is_some_and(|t| {
            now.duration_since(t)
                .map_or(true, |age| age < MIN_FS_ORPHAN_AGE)
        });
        if too_new && !linked {
            // Treat like a linked record: keep it and its file.
            if let Some(p) = &path {
                kept_files.insert(path_key(p));
            }
            continue;
        }
        if linked || (is_retained && !include_retained) {
            if let Some(p) = &path {
                kept_files.insert(path_key(p));
                if is_retained && is_within_roots(p, roots) {
                    out.retained_bytes = out.retained_bytes.saturating_add(file_len(p));
                }
            }
            if is_retained {
                out.retained_count += 1;
            }
        } else {
            if is_retained {
                out.included_retained.push(rowid);
            }
            candidates.push((rowid, path));
        }
    }
    // Only forget ids whose record is gone. A retained id that is linked
    // right now may belong to a messages-only delete that is about to
    // commit; dropping it would leave the attachment unprotected.
    out.stale_retained = retained
        .iter()
        .copied()
        .filter(|id| !existing.contains(id))
        .collect();

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
            if referenced_files.contains(&path_key(&path))
                || in_referenced_dir(&path, root, &referenced_dirs)
                || is_recent(&path, now)
            {
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
pub async fn scan_orphans(
    app: AppHandle,
    lock: State<'_, OperationLock>,
    include_retained: bool,
) -> Result<OrphanScan, AppError> {
    let _guard = lock.try_acquire("an orphan scan")?;
    let store = RetainedAttachments::for_app(&app)?;
    blocking(move || scan_orphans_blocking(&store, include_retained)).await
}

pub(crate) fn scan_orphans_blocking(
    store: &RetainedAttachments,
    include_retained: bool,
) -> Result<OrphanScan, AppError> {
    let db_path = default_chat_db_path()?;
    let conn = open_read(&db_path)?;
    let retained = store.load()?;
    let orphans = find_orphans(
        &conn,
        &retained,
        include_retained,
        &attachment_roots(),
        SystemTime::now(),
    )?;
    // Housekeeping: forget kept attachments that were since deleted or
    // linked to a message again, so the list doesn't grow forever.
    store.remove(&orphans.stale_retained)?;

    Ok(OrphanScan {
        db_orphan_count: orphans.db.len() as u64,
        db_orphan_bytes: orphans.db_bytes,
        fs_orphan_count: orphans.fs.len() as u64,
        fs_orphan_bytes: orphans.fs_bytes,
        retained_count: orphans.retained_count,
        retained_bytes: orphans.retained_bytes,
        fingerprint: orphans.fingerprint(),
    })
}

/// Delete the given orphan rows in one transaction, re-checking the orphan
/// condition so a row that gained a link since the scan is left alone.
/// Returns the rowids whose record is actually gone.
fn delete_orphan_rows(conn: &mut Connection, rows: &[DbOrphan]) -> Result<HashSet<i64>, AppError> {
    let mut deleted = HashSet::new();
    let tx = conn.transaction()?;
    for o in rows {
        tx.execute(
            "DELETE FROM attachment WHERE ROWID = ?1 \
             AND NOT EXISTS (SELECT 1 FROM message_attachment_join WHERE attachment_id = ?1)",
            rusqlite::params![o.rowid],
        )?;
        // Check directly rather than trusting the affected-row count:
        // chat.db's delete triggers can make it unreliable.
        let still_there: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM attachment WHERE ROWID = ?1)",
            rusqlite::params![o.rowid],
            |r| r.get(0),
        )?;
        if !still_there {
            deleted.insert(o.rowid);
        }
    }
    tx.commit()?;
    Ok(deleted)
}

fn ensure_messages_not_running(messages_running: fn() -> bool) -> Result<(), AppError> {
    if messages_running() {
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
    lock: State<'_, OperationLock>,
    args: CleanOrphansArgs,
) -> Result<OrphanCleanResult, AppError> {
    let _guard = lock.try_acquire("a cleanup")?;
    let store = RetainedAttachments::for_app(&app)?;
    let snapshot_root = default_snapshot_root(&app, SnapshotKind::OrphanClean)?;
    blocking(move || clean_orphans_blocking(&store, &snapshot_root, &args, &SafetyProbe::system()))
        .await
}

pub(crate) fn clean_orphans_blocking(
    store: &RetainedAttachments,
    snapshot_root: &Path,
    args: &CleanOrphansArgs,
    probe: &SafetyProbe,
) -> Result<OrphanCleanResult, AppError> {
    ensure_messages_not_running(probe.messages_running)?;

    let db_path = default_chat_db_path()?;
    let roots = attachment_roots();
    let retained = store.load()?;

    let orphans = {
        let conn = open_read(&db_path)?;
        find_orphans(
            &conn,
            &retained,
            args.include_retained,
            &roots,
            SystemTime::now(),
        )?
        // conn dropped here
    };
    if orphans.fingerprint() != args.scan_fingerprint {
        return Err(AppError::Other(
            "Orphaned data changed since the scan. Run the scan again and review the new \
             results before cleaning."
                .into(),
        ));
    }
    if !orphans.db.is_empty()
        && (probe.icloud)() != ICloudState::Disabled
        && !args.acknowledge_icloud_sync
    {
        return Err(AppError::Other(
            "Removing attachment records queues them for deletion in Messages in iCloud, which \
             can remove them from your other devices too. Acknowledge the iCloud warning in the \
             cleanup dialog (or pass acknowledgeIcloudSync=true) before retrying."
                .into(),
        ));
    }

    let mut db_rows_deleted = 0u64;
    let mut snapshot_path = None;
    // Rowids whose record really was deleted. Only their files are removed:
    // a row the re-check below kept (it gained a link since the scan) must
    // keep its file too.
    let mut deleted: HashSet<i64> = HashSet::new();
    if !orphans.db.is_empty() {
        checkpoint_wal(&db_path)?;
        let snapshot = snapshot_chat_db(&db_path, snapshot_root)?;

        // If anything fails before the rows are deleted, nothing changed:
        // drop the redundant snapshot instead of letting it push an older
        // one out, and only prune after a successful clean.
        let result = (|| {
            ensure_messages_not_running(probe.messages_running)?;
            let mut conn = open_write(&db_path)?;
            delete_orphan_rows(&mut conn, &orphans.db)
        })();
        deleted = match result {
            Ok(d) => d,
            Err(e) => {
                if let Err(re) = std::fs::remove_dir_all(&snapshot.dir) {
                    eprintln!("orphan-clean: couldn't remove unused snapshot: {re}");
                }
                return Err(e);
            }
        };
        prune_snapshots(snapshot_root, SNAPSHOTS_TO_KEEP, &snapshot.dir);
        snapshot_path = Some(snapshot.dir.display().to_string());
        db_rows_deleted = deleted.len() as u64;
        // The included kept attachments that are gone now; stop tracking
        // them.
        let gone: Vec<i64> = orphans
            .included_retained
            .iter()
            .copied()
            .filter(|id| deleted.contains(id))
            .collect();
        store.remove(&gone)?;
    }

    let mut files_skipped = 0u64;
    let mut db_files_removed = 0u64;
    let mut db_files_failed = 0u64;
    let deleted_files = orphans
        .db
        .iter()
        .filter(|o| deleted.contains(&o.rowid))
        .filter_map(|o| o.path.as_deref());
    for path in deleted_files {
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
            "CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY, filename TEXT, created_date INTEGER DEFAULT 0);
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

        let o = find_orphans(&conn, &BTreeSet::new(), false, &[root], later()).unwrap();
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

        let o = find_orphans(&conn, &BTreeSet::new(), false, &[root], later()).unwrap();
        assert!(o.fs.is_empty());
    }

    #[test]
    fn recent_files_are_not_fs_orphans() {
        let tmp = TempDir::new().unwrap();
        let (conn, root) = fixture(&tmp);
        file(&root.join("ab/01/GUID-NEW/incoming.jpg"));

        let o = find_orphans(&conn, &BTreeSet::new(), false, &[root], SystemTime::now()).unwrap();
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

        let o = find_orphans(
            &conn,
            &retained,
            false,
            std::slice::from_ref(&root),
            later(),
        )
        .unwrap();
        let got: Vec<(i64, Option<PathBuf>)> =
            o.db.iter().map(|d| (d.rowid, d.path.clone())).collect();
        // Row 5 is retained, so its file (shared with orphan row 3) is kept.
        assert_eq!(got, vec![(2, None), (3, None), (4, None), (6, Some(solo))]);
        assert_eq!(o.retained_count, 1);
        assert!(o.included_retained.is_empty());

        // Opting in turns the kept attachment (and the file it shares with
        // row 3) into cleanup candidates.
        let o = find_orphans(&conn, &retained, true, &[root], later()).unwrap();
        let ids: Vec<i64> = o.db.iter().map(|d| d.rowid).collect();
        assert_eq!(ids, vec![2, 3, 4, 5, 6]);
        assert_eq!(o.included_retained, vec![5]);
        assert_eq!(o.retained_count, 0);
        assert!(o
            .db
            .iter()
            .any(|d| d.rowid == 3 && d.path == Some(own.clone())));
    }

    #[test]
    fn rows_linked_since_the_scan_are_not_deleted() {
        let tmp = TempDir::new().unwrap();
        let (mut conn, root) = fixture(&tmp);
        let a = file(&root.join("ab/01/G/a.jpg"));
        let b = file(&root.join("ab/02/G/b.jpg"));
        add_row(&conn, 1, &a, false);
        add_row(&conn, 2, &b, false);
        let o = find_orphans(
            &conn,
            &BTreeSet::new(),
            false,
            std::slice::from_ref(&root),
            later(),
        )
        .unwrap();
        assert_eq!(o.db.len(), 2);

        // Row 2 gets linked to a message after the scan.
        conn.execute("INSERT INTO message_attachment_join VALUES (5, 2)", [])
            .unwrap();
        let deleted = delete_orphan_rows(&mut conn, &o.db).unwrap();
        assert_eq!(deleted, HashSet::from([1]));
    }

    #[test]
    fn stale_retained_ids_are_reported() {
        let tmp = TempDir::new().unwrap();
        let (conn, root) = fixture(&tmp);
        let a = file(&root.join("ab/01/G/a.jpg"));
        add_row(&conn, 1, &a, true); // linked (e.g. a delete mid-commit): keep tracking
        add_row(&conn, 2, &a, false); // still kept & unlinked
        let retained = BTreeSet::from([1, 2, 99]); // 99 no longer exists

        let o = find_orphans(&conn, &retained, false, &[root], later()).unwrap();
        assert_eq!(o.stale_retained, vec![99]);
    }

    #[test]
    fn fingerprint_changes_when_orphans_change() {
        let tmp = TempDir::new().unwrap();
        let (conn, root) = fixture(&tmp);
        let a = file(&root.join("ab/01/G/a.jpg"));
        add_row(&conn, 1, &a, false);
        let roots = [root];
        let f1 = find_orphans(&conn, &BTreeSet::new(), false, &roots, later())
            .unwrap()
            .fingerprint();
        let f2 = find_orphans(&conn, &BTreeSet::new(), false, &roots, later())
            .unwrap()
            .fingerprint();
        assert_eq!(f1, f2);

        add_row(&conn, 2, &a, false);
        let f3 = find_orphans(&conn, &BTreeSet::new(), false, &roots, later())
            .unwrap()
            .fingerprint();
        assert_ne!(f1, f3);
    }
}
